//! WASAPI shared low-latency capture.
//!
//! Uses `IAudioClient3::InitializeSharedAudioStream` so we don't take
//! exclusive ownership of the mic (other apps can still record, the system
//! mixer still works). The capture loop is event-driven: we wait on the
//! buffer-ready event the audio engine signals every period.
//!
//! Output of this module is always **mono f32 @ 48 kHz, 480-sample frames**.
//! If the device's mix format differs we down-mix and (linear) resample
//! inline. Linear resampling is fine for a near-rate match (most modern
//! mics already negotiate 48 kHz); for 44.1 → 48 we sound a small quality
//! cost for a much lighter dependency than rubato. Swap to `rubato` if
//! quality matters.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;

use crate::devices::{Device, DeviceDirection, DeviceList};
use crate::error::{AudioError, Result};
use crate::{FRAME_SAMPLES, SAMPLE_RATE};

#[allow(non_upper_case_globals)]
const CLSID_MMDeviceEnumerator: windows::core::GUID =
    windows::core::GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);

/// Frame produced by the capture loop: always 480 mono f32 samples.
pub type Frame = [f32; FRAME_SAMPLES];

/// Callback the capture thread invokes every 10 ms with a fresh frame.
/// Must be cheap and non-blocking — push to a ring buffer and return.
pub trait FrameSink: Send {
    fn on_frame(&mut self, frame: &Frame);
    /// Called when the audio engine reports glitches (data discontinuity,
    /// timestamp error, silent fill). Useful for logging xruns.
    fn on_glitch(&mut self, _flags: u32) {}
}

pub struct WasapiCapture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WasapiCapture {
    /// Open the given capture device and start delivering 480-sample frames
    /// to `sink`. The capture runs on a dedicated MMCSS "Pro Audio" thread.
    pub fn start(device_id: &str, mut sink: Box<dyn FrameSink>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let device_id = device_id.to_string();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();

        let thread = std::thread::Builder::new()
            .name("noisegate-capture".into())
            .spawn(move || {
                let res = capture_loop(&device_id, &mut *sink, &stop_thread, &ready_tx);
                if let Err(e) = res {
                    tracing::error!(error = %e, "capture loop exited with error");
                    // ready_tx may already have been signaled; ignore.
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| AudioError::Other(anyhow::anyhow!("spawn capture thread: {e}")))?;

        // Wait for the thread to finish init so callers learn about open/format
        // failures synchronously.
        ready_rx
            .recv()
            .map_err(|_| AudioError::ThreadDied)??;

        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for WasapiCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn capture_loop(
    device_id: &str,
    sink: &mut dyn FrameSink,
    stop: &AtomicBool,
    ready_tx: &std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    unsafe {
        // COM init for this thread. STA isn't required for WASAPI; MTA is
        // simpler and matches the audio engine's threading model.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&CLSID_MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| AudioError::wasapi("CoCreateInstance(MMDeviceEnumerator)", e))?;

        let device = find_device(&enumerator, device_id, eCapture)?;

        let client: IAudioClient3 = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| AudioError::wasapi("IMMDevice::Activate", e))?;

        // GetMixFormat returns a CoTaskMem-allocated WAVEFORMATEX. On most
        // devices this is actually a WAVEFORMATEXTENSIBLE (40 bytes, with
        // cbSize > 0). We MUST pass the original pointer back to Initialize
        // — copying it into a 18-byte WAVEFORMATEX truncates the extensible
        // header and the engine rejects it with E_INVALIDARG.
        let mix_ptr = client
            .GetMixFormat()
            .map_err(|e| AudioError::wasapi("GetMixFormat", e))?;

        // Snapshot the fields we need into locals (Copy), so we can drop
        // the pointer after Initialize and not worry about packed-struct
        // unaligned-reference issues elsewhere in the loop.
        let device_rate = (*mix_ptr).nSamplesPerSec;
        let device_channels = (*mix_ptr).nChannels as usize;
        let needs_convert = !(device_rate == SAMPLE_RATE && device_channels == 1);

        if needs_convert {
            tracing::info!(
                device_rate,
                device_channels,
                "device mix format differs from pipeline; converting inline"
            );
        }

        // Legacy Initialize — accepts the device's native (possibly
        // extensible) mix format reliably. Buffer duration 0 = engine
        // default (~30 ms), fine for voice.
        let init_res = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0,                  // hnsBufferDuration
            0,                  // hnsPeriodicity (must be 0 in shared mode)
            mix_ptr,            // pass the original pointer through
            None,
        );
        // Always free the format pointer, regardless of success/failure.
        windows::Win32::System::Com::CoTaskMemFree(Some(mix_ptr as _));
        init_res.map_err(|e| AudioError::wasapi("IAudioClient::Initialize", e))?;

        let event = CreateEventW(None, false, false, PCWSTR::null())
            .map_err(|e| AudioError::wasapi("CreateEventW", e))?;
        client
            .SetEventHandle(event)
            .map_err(|e| AudioError::wasapi("SetEventHandle", e))?;

        let cap_client: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| AudioError::wasapi("GetService(IAudioCaptureClient)", e))?;

        // MMCSS: ask the scheduler to treat this as Pro Audio. Without this,
        // we'll get random scheduling delays under load and audible glitches.
        let _mmcss = crate::mmcss::ProAudio::set_for_current_thread();

        client
            .Start()
            .map_err(|e| AudioError::wasapi("IAudioClient::Start", e))?;

        // Init succeeded — unblock the caller.
        let _ = ready_tx.send(Ok(()));

        let mut accumulator = FrameAccumulator::new();
        let mut converter = if needs_convert {
            Some(InlineConverter::new(device_rate, device_channels, SAMPLE_RATE))
        } else {
            None
        };

        while !stop.load(Ordering::Acquire) {
            let wait = WaitForSingleObject(event, 200 /* ms */);
            if wait != WAIT_OBJECT_0 {
                continue; // timeout — re-check stop flag
            }

            // Drain everything the engine has for us this tick.
            loop {
                let mut buffer_ptr: *mut u8 = std::ptr::null_mut();
                let mut frames_avail: u32 = 0;
                let mut flags: u32 = 0;
                let r = cap_client.GetBuffer(
                    &mut buffer_ptr,
                    &mut frames_avail,
                    &mut flags,
                    None,
                    None,
                );
                if let Err(e) = r {
                    // AUDCLNT_S_BUFFER_EMPTY is informational, not an error.
                    if e.code() == windows::Win32::Media::Audio::AUDCLNT_S_BUFFER_EMPTY {
                        break;
                    }
                    return Err(AudioError::wasapi("GetBuffer", e));
                }
                if frames_avail == 0 {
                    let _ = cap_client.ReleaseBuffer(0);
                    break;
                }

                if flags
                    & (AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0
                        | AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0
                        | AUDCLNT_BUFFERFLAGS_SILENT.0) as u32
                    != 0
                {
                    sink.on_glitch(flags);
                }

                // The buffer matches the device's mix format (mono or
                // multi-channel f32 / int — engines invariably hand us f32
                // for shared streams).
                let sample_count = frames_avail as usize * device_channels;
                let raw =
                    std::slice::from_raw_parts(buffer_ptr as *const f32, sample_count);

                let mono_48k: &[f32] = match converter.as_mut() {
                    None => raw,
                    Some(c) => c.process(raw, frames_avail as usize),
                };

                accumulator.feed(mono_48k, |frame| sink.on_frame(frame));

                cap_client
                    .ReleaseBuffer(frames_avail)
                    .map_err(|e| AudioError::wasapi("ReleaseBuffer", e))?;
            }
        }

        let _ = client.Stop();
        Ok(())
    }
}

/// Public for `devices::DeviceList::enumerate()`.
pub(crate) fn enumerate_all() -> Result<DeviceList> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&CLSID_MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| AudioError::wasapi("CoCreateInstance(MMDeviceEnumerator)", e))?;

        let mut list = DeviceList::default();
        list.capture = enumerate_direction(&enumerator, eCapture)?;
        list.render = enumerate_direction(&enumerator, eRender)?;
        Ok(list)
    }
}

unsafe fn enumerate_direction(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
) -> Result<Vec<Device>> {
    let coll = enumerator
        .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
        .map_err(|e| AudioError::wasapi("EnumAudioEndpoints", e))?;

    let default_id = enumerator
        .GetDefaultAudioEndpoint(flow, eCommunications)
        .ok()
        .and_then(|d| d.GetId().ok())
        .map(|p| p.to_string().unwrap_or_default())
        .unwrap_or_default();

    let count = coll.GetCount().map_err(|e| AudioError::wasapi("GetCount", e))?;
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let dev = coll.Item(i).map_err(|e| AudioError::wasapi("Item", e))?;
        let id = dev
            .GetId()
            .map_err(|e| AudioError::wasapi("GetId", e))?
            .to_string()
            .unwrap_or_default();
        let friendly_name = read_friendly_name(&dev).unwrap_or_else(|_| id.clone());
        out.push(Device {
            is_default: id == default_id,
            id,
            friendly_name,
            direction: match flow {
                EDataFlow(0) => DeviceDirection::Render,
                _ => DeviceDirection::Capture,
            },
        });
    }
    Ok(out)
}

unsafe fn read_friendly_name(dev: &IMMDevice) -> Result<String> {
    let store = dev
        .OpenPropertyStore(STGM_READ)
        .map_err(|e| AudioError::wasapi("OpenPropertyStore", e))?;
    // windows 0.58: GetValue returns the PROPVARIANT directly.
    let prop = store
        .GetValue(&PKEY_Device_FriendlyName)
        .map_err(|e| AudioError::wasapi("GetValue(FriendlyName)", e))?;
    // PROPVARIANT for a string is VT_LPWSTR; Display impl decodes it.
    Ok(prop.to_string())
}

unsafe fn find_device(
    enumerator: &IMMDeviceEnumerator,
    id: &str,
    flow: EDataFlow,
) -> Result<IMMDevice> {
    if id.is_empty() || id == "default" {
        return enumerator
            .GetDefaultAudioEndpoint(flow, eCommunications)
            .map_err(|e| AudioError::wasapi("GetDefaultAudioEndpoint", e));
    }
    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    enumerator
        .GetDevice(PCWSTR::from_raw(wide.as_ptr()))
        .map_err(|e| AudioError::wasapi("GetDevice", e))
}

/// Accumulates an arbitrary-length mono f32 stream into fixed 480-sample
/// frames. Holds at most one partial frame across calls.
pub(crate) struct FrameAccumulator {
    buf: Vec<f32>,
}

impl FrameAccumulator {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(FRAME_SAMPLES * 2),
        }
    }

    pub fn feed(&mut self, samples: &[f32], mut emit: impl FnMut(&Frame)) {
        let mut i = 0;
        while i < samples.len() {
            let need = FRAME_SAMPLES - self.buf.len();
            let take = need.min(samples.len() - i);
            self.buf.extend_from_slice(&samples[i..i + take]);
            i += take;
            if self.buf.len() == FRAME_SAMPLES {
                let mut frame = [0f32; FRAME_SAMPLES];
                frame.copy_from_slice(&self.buf);
                self.buf.clear();
                emit(&frame);
            }
        }
    }
}

/// Cheap inline downmix + linear resampler. Good enough for voice; replace
/// with `rubato` if you ever want to ship music-grade quality.
pub(crate) struct InlineConverter {
    src_rate: u32,
    src_channels: usize,
    dst_rate: u32,
    last_sample: f32,
    /// Fractional position into the source stream; advanced by src/dst per
    /// output sample.
    phase: f64,
    out: Vec<f32>,
}

impl InlineConverter {
    pub fn new(src_rate: u32, src_channels: usize, dst_rate: u32) -> Self {
        Self {
            src_rate,
            src_channels,
            dst_rate,
            last_sample: 0.0,
            phase: 0.0,
            out: Vec::with_capacity(2048),
        }
    }

    pub fn process(&mut self, interleaved: &[f32], frames: usize) -> &[f32] {
        // Step 1: downmix to mono in a scratch buffer of length `frames`.
        let mut mono = Vec::with_capacity(frames);
        if self.src_channels == 1 {
            mono.extend_from_slice(&interleaved[..frames]);
        } else {
            for f in 0..frames {
                let base = f * self.src_channels;
                let mut acc = 0.0f32;
                for c in 0..self.src_channels {
                    acc += interleaved[base + c];
                }
                mono.push(acc / self.src_channels as f32);
            }
        }

        // Step 2: linear-resample mono → dst_rate.
        self.out.clear();
        if self.src_rate == self.dst_rate {
            self.out.extend_from_slice(&mono);
            self.last_sample = *mono.last().unwrap_or(&self.last_sample);
            return &self.out;
        }

        let ratio = self.src_rate as f64 / self.dst_rate as f64;
        let total_src = mono.len() as f64;
        while self.phase < total_src {
            let idx = self.phase as usize;
            let frac = self.phase - idx as f64;
            let a = if idx == 0 { self.last_sample } else { mono[idx - 1] };
            let b = mono.get(idx).copied().unwrap_or(self.last_sample);
            self.out.push((a as f64 + (b as f64 - a as f64) * frac) as f32);
            self.phase += ratio;
        }
        self.phase -= total_src;
        if let Some(&s) = mono.last() {
            self.last_sample = s;
        }
        &self.out
    }
}

