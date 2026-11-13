//! Pipeline glue: capture → ring A → DSP thread → ring B → render.
//!
//! Two SPSC ring buffers connect the three audio threads. We size the rings
//! at 8 frames (~80 ms) — enough headroom to absorb a scheduler hiccup,
//! small enough that we don't hide actual problems.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;
use tracing::{info, warn};

use audio_io::{devices::DeviceList, wasapi_capture::Frame};
use dsp::{DenoiserHost, Stats};

use crate::config::Config;
use crate::parking_lot_compat::RwLock;

const RING_FRAMES: usize = 8;

pub struct Pipeline {
    /// Held to keep the audio threads alive. Dropping these stops them.
    #[allow(dead_code)]
    capture: audio_io::WasapiCapture,
    #[allow(dead_code)]
    render: audio_io::WasapiRender,
    #[allow(dead_code)]
    dsp_thread: Option<std::thread::JoinHandle<()>>,

    bypass: Arc<AtomicBool>,
    stats: Arc<Stats>,
    denoiser_name: &'static str,
    /// Used to ask the DSP thread to exit cleanly when we drop.
    shutdown: Arc<AtomicBool>,
    output: Output,
    rate_mismatch: Option<RateMismatch>,
}

/// The virtual cable's speaker and mic sides use different formats (sample
/// rate and/or channel count).
#[derive(Debug, Clone)]
pub struct RateMismatch {
    pub mic_name: String,
    pub mic_hz: u32,
    pub mic_ch: u16,
    pub speaker_hz: u32,
    pub speaker_ch: u16,
}

impl RateMismatch {
    fn fmt(hz: u32, ch: u16) -> String {
        let chs = match ch {
            1 => "mono".to_string(),
            2 => "stereo".to_string(),
            n => format!("{n} ch"),
        };
        format!("{} Hz {}", hz, chs)
    }
    pub fn mic_format(&self) -> String {
        Self::fmt(self.mic_hz, self.mic_ch)
    }
    pub fn speaker_format(&self) -> String {
        Self::fmt(self.speaker_hz, self.speaker_ch)
    }
}

/// Where the cleaned audio goes.
#[derive(Debug, Clone, Copy)]
pub enum Output {
    /// A virtual cable: other apps can use its mic side.
    Cable(&'static audio_io::devices::VirtualCable),
    /// No cable found; playing on the default speakers (not usable as a mic).
    DefaultSpeakers,
    /// A device the user picked explicitly (--out / config).
    Manual,
}

impl Pipeline {
    pub fn start(cfg: Arc<RwLock<Config>>) -> Result<Self> {
        let snapshot = cfg.read().unwrap().clone();

        // Resolve devices.
        let devices = DeviceList::enumerate().context("enumerating audio devices")?;
        let input_id = if snapshot.input_device_id.is_empty() {
            devices
                .default_capture()
                .map(|d| d.id.clone())
                .unwrap_or_default()
        } else {
            snapshot.input_device_id.clone()
        };
        let output_id = if snapshot.output_device_id.is_empty() {
            match devices.find_virtual_cable() {
                Some((d, cable)) => {
                    info!(
                        cable = cable.label,
                        output = %d.friendly_name,
                        "virtual cable found — select \"{}\" as your microphone in Discord/Zoom/Teams/OBS",
                        cable.mic_name
                    );
                    d.id.clone()
                }
                None => {
                    warn!(
                        "no virtual cable found (VB-Cable, VoiceMeeter or Steam Streaming Microphone); \
                         playing cleaned audio on the default output device instead. Other apps \
                         can't use it as a mic like this. Install VB-Cable, or pick an output \
                         with --out (e.g. an audio interface output that has a hardware loopback)."
                    );
                    String::new()
                }
            }
        } else {
            snapshot.output_device_id.clone()
        };
        // Which virtual cable (if any) the output is — drives the tray's
        // "install a virtual mic" guidance.
        let cable = devices
            .render
            .iter()
            .find(|d| d.id == output_id)
            .and_then(|d| d.virtual_cable());
        let output = match (cable, output_id.is_empty()) {
            (Some(c), _) => Output::Cable(c),
            (None, true) => Output::DefaultSpeakers,
            (None, false) => Output::Manual,
        };

        // Pass-through cables (Steam's, at least) copy raw samples between
        // their two ends without converting. If the speaker side is 48 kHz
        // stereo and the mic side 44.1 kHz or mono, other apps hear us slowed
        // down — measured: a 1 kHz tone came out at ~508 Hz on a stereo→mono
        // Steam cable. Only the user can change an endpoint's format, so
        // detect it and tell them.
        let rate_mismatch = match output {
            Output::Cable(c) => {
                use audio_io::{wasapi_capture::endpoint_format, DeviceDirection};
                let mic = devices.capture.iter().find(|d| c.matches_name(&d.friendly_name));
                let spk = endpoint_format(&output_id, DeviceDirection::Render);
                match (mic, spk) {
                    (Some(mic), Ok((spk_hz, spk_ch))) => {
                        match endpoint_format(&mic.id, DeviceDirection::Capture) {
                            Ok((mic_hz, mic_ch)) if mic_hz != spk_hz || mic_ch != spk_ch => {
                                warn!(
                                    speaker_side_hz = spk_hz,
                                    speaker_side_channels = spk_ch,
                                    mic_side_hz = mic_hz,
                                    mic_side_channels = mic_ch,
                                    "virtual cable's two ends use different formats — other \
                                     apps will hear you pitched down and warped. Set \"{}\" to \
                                     {} channel(s), {} Hz in Sound settings (mmsys.cpl → \
                                     Recording → Properties → Advanced), or use VB-Cable.",
                                    mic.friendly_name,
                                    spk_ch,
                                    spk_hz
                                );
                                Some(RateMismatch {
                                    mic_name: mic.friendly_name.clone(),
                                    mic_hz,
                                    mic_ch,
                                    speaker_hz: spk_hz,
                                    speaker_ch: spk_ch,
                                })
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        };

        // Capturing from the mic side of the cable we render into would feed
        // our own output straight back into the input.
        let name_of = |list: &[audio_io::Device], id: &str| {
            list.iter().find(|d| d.id == id).map(|d| d.friendly_name.clone())
        };
        if let (Some(in_name), Some(out_name)) =
            (name_of(&devices.capture, &input_id), name_of(&devices.render, &output_id))
        {
            let same_cable = audio_io::devices::VIRTUAL_CABLES
                .iter()
                .any(|c| c.matches_name(&in_name) && c.matches_name(&out_name));
            if same_cable {
                anyhow::bail!(
                    "input \"{in_name}\" is the other end of output \"{out_name}\" — that would \
                     loop NoiseGate's output back into itself. Pick your real mic with --mic."
                );
            }
        }

        info!(input = %input_id, output = %output_id, "resolved audio devices");

        // Build the rings.
        let (prod_a, mut cons_a) = HeapRb::<Frame>::new(RING_FRAMES).split();
        let (mut prod_b, cons_b) = HeapRb::<Frame>::new(RING_FRAMES).split();

        // DSP setup.
        let denoiser = dsp::denoiser_by_key(&snapshot.denoiser, snapshot.attenuation_db)
            .context("loading denoiser")?;
        let denoiser_name = denoiser.name();
        let (mut host, bypass, stats) = DenoiserHost::new(denoiser);
        bypass.store(!snapshot.enabled, Ordering::Relaxed);

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_dsp = shutdown.clone();

        // DSP thread: pulls from ring A, processes, pushes to ring B.
        let dsp_thread = std::thread::Builder::new()
            .name("noisegate-dsp".into())
            .spawn(move || {
                #[cfg(windows)]
                let _mmcss = audio_io::mmcss_pro_audio_for_current_thread();
                // An empty ring on a single poll is normal: we poll every 2 ms
                // and capture delivers a frame every 10 ms. Only a gap well
                // past one frame period means capture has actually stalled.
                const STARVE_GAP: std::time::Duration = std::time::Duration::from_millis(100);
                let test_tone_hz: Option<f64> = std::env::var("NOISEGATE_TEST_TONE")
                    .ok()
                    .and_then(|v| v.parse().ok());
                let mut tone_phase = 0.0f64;
                // NOISEGATE_TEST_TONE_DB sets the level (dBFS), default -40 so a
                // monitored device doesn't blast anyone's ears.
                let tone_amp: f32 = 10f32.powf(
                    std::env::var("NOISEGATE_TEST_TONE_DB")
                        .ok()
                        .and_then(|v| v.parse::<f32>().ok())
                        .unwrap_or(-40.0)
                        .min(-12.0)
                        / 20.0,
                ) * std::f32::consts::SQRT_2;
                let mut last_frame = std::time::Instant::now();
                let mut last_starve_log = std::time::Instant::now()
                    - std::time::Duration::from_secs(60); // force first one to log
                while !shutdown_dsp.load(Ordering::Acquire) {
                    let mut frame = match cons_a.try_pop() {
                        Some(f) => {
                            last_frame = std::time::Instant::now();
                            f
                        }
                        None => {
                            let gap = last_frame.elapsed();
                            // Log on the first stall in a 5-second window so
                            // the issue is visible without spamming.
                            if gap > STARVE_GAP
                                && last_starve_log.elapsed() > std::time::Duration::from_secs(5)
                            {
                                warn!(
                                    gap_ms = gap.as_millis() as u64,
                                    "DSP thread starved — capture not delivering frames"
                                );
                                last_starve_log = std::time::Instant::now();
                            }
                            std::thread::sleep(std::time::Duration::from_millis(2));
                            continue;
                        }
                    };
                    // Diagnostic: NOISEGATE_TEST_TONE=<Hz> replaces the mic with
                    // a sine (after capture, skipping the denoiser) so the
                    // output chain's pitch can be measured.
                    if let Some(hz) = test_tone_hz {
                        for s in frame.iter_mut() {
                            *s = tone_amp * (tone_phase * std::f64::consts::TAU).sin() as f32;
                            tone_phase = (tone_phase + hz / 48_000.0).fract();
                        }
                        if prod_b.try_push(frame).is_err() {
                            warn!("ring B full; dropping a frame");
                        }
                        continue;
                    }
                    if let Err(e) = host.process(&mut frame) {
                        warn!(error = %e, "denoiser error; passing frame through");
                    }
                    if prod_b.try_push(frame).is_err() {
                        // Render is behind — drop. Audible as a click; better
                        // than blocking the DSP thread.
                        warn!("ring B full; dropping a frame");
                    }
                }
            })
            .context("spawn dsp thread")?;
        // From here on, any early return must stop the DSP thread or it spins
        // forever on an empty ring. The guard does that on drop; it is
        // disarmed once the Pipeline owns the thread.
        let dsp_guard = DspGuard { shutdown: shutdown.clone(), thread: Some(dsp_thread) };

        // Capture sink: pushes into ring A.
        struct Sink<P: Producer<Item = Frame> + Send> {
            prod: P,
        }
        impl<P: Producer<Item = Frame> + Send> audio_io::wasapi_capture::FrameSink for Sink<P> {
            fn on_frame(&mut self, frame: &Frame) {
                if self.prod.try_push(*frame).is_err() {
                    // DSP behind — overwrite oldest by popping (we don't
                    // have direct access here; the simplest cheap option is
                    // to just drop. Audible as a tiny click; far better
                    // than blocking the audio engine.)
                    tracing::warn!("ring A full; dropping captured frame");
                }
            }
            fn on_glitch(&mut self, flags: u32) {
                tracing::warn!(flags, "capture glitch reported by audio engine");
            }
        }

        let capture = audio_io::WasapiCapture::start(
            &input_id,
            Box::new(Sink { prod: prod_a }),
        )
        .map_err(|e| anyhow::anyhow!(e))?;

        // Render source: pulls from ring B.
        struct Source<C: Consumer<Item = Frame> + Send> {
            cons: C,
            last_underrun_log: std::time::Instant,
        }
        impl<C: Consumer<Item = Frame> + Send> audio_io::wasapi_render::FrameSource for Source<C> {
            fn next_frame(&mut self) -> Option<Frame> {
                self.cons.try_pop()
            }
            fn on_underrun(&mut self) {
                if self.last_underrun_log.elapsed() > std::time::Duration::from_secs(5) {
                    tracing::warn!("render underrun (cleaned audio not arriving from DSP)");
                    self.last_underrun_log = std::time::Instant::now();
                }
            }
        }

        let render = audio_io::WasapiRender::start(
            &output_id,
            Box::new(Source {
                cons: cons_b,
                last_underrun_log: std::time::Instant::now()
                    - std::time::Duration::from_secs(60),
            }),
        )
        .map_err(|e| anyhow::anyhow!(e))?;

        Ok(Self {
            capture,
            render,
            dsp_thread: Some(dsp_guard.disarm()),
            bypass,
            stats,
            denoiser_name,
            shutdown,
            output,
            rate_mismatch,
        })
    }

    /// Both WASAPI streams still running. False after a device error; the
    /// tray then restarts the pipeline.
    pub fn is_healthy(&self) -> bool {
        self.capture.is_alive() && self.render.is_alive()
    }

    pub fn output(&self) -> Output {
        self.output
    }

    pub fn rate_mismatch(&self) -> Option<&RateMismatch> {
        self.rate_mismatch.as_ref()
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.bypass.store(!enabled, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        !self.bypass.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn denoiser_name(&self) -> &'static str {
        self.denoiser_name
    }
}

/// Stops and joins the DSP thread if `Pipeline::start` bails after spawning it.
struct DspGuard {
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DspGuard {
    /// Hand the thread over to its permanent owner; drop becomes a no-op.
    fn disarm(mut self) -> std::thread::JoinHandle<()> {
        self.thread.take().expect("DspGuard disarmed twice")
    }
}

impl Drop for DspGuard {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            self.shutdown.store(true, Ordering::Release);
            let _ = t.join();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(t) = self.dsp_thread.take() {
            let _ = t.join();
        }
    }
}
