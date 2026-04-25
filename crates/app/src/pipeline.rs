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
            match devices.find_vb_cable_input() {
                Ok(d) => d.id.clone(),
                Err(_) => {
                    warn!("VB-Cable not found; falling back to default render device. Install VB-Cable for proper integration with Zoom/Teams/Discord.");
                    String::new()
                }
            }
        } else {
            snapshot.output_device_id.clone()
        };

        info!(input = %input_id, output = %output_id, "resolved audio devices");

        // Build the rings.
        let (prod_a, mut cons_a) = HeapRb::<Frame>::new(RING_FRAMES).split();
        let (mut prod_b, cons_b) = HeapRb::<Frame>::new(RING_FRAMES).split();

        // DSP setup.
        let denoiser = dsp::default_denoiser().context("loading denoiser")?;
        let denoiser_name = denoiser.name();
        let (mut host, bypass, stats) = DenoiserHost::new(denoiser);
        bypass.store(!snapshot.enabled, Ordering::Relaxed);

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_dsp = shutdown.clone();
        let stats_for_thread = stats.clone();

        // DSP thread: pulls from ring A, processes, pushes to ring B.
        let dsp_thread = std::thread::Builder::new()
            .name("noisegate-dsp".into())
            .spawn(move || {
                #[cfg(windows)]
                let _mmcss = audio_io::mmcss_pro_audio_for_current_thread();
                let _ = stats_for_thread; // keep alive via host
                let mut starved = 0u64;
                while !shutdown_dsp.load(Ordering::Acquire) {
                    let mut frame = match cons_a.try_pop() {
                        Some(f) => f,
                        None => {
                            starved += 1;
                            if starved % 200 == 0 {
                                warn!(starved, "DSP thread starved (capture not delivering)");
                            }
                            std::thread::sleep(std::time::Duration::from_millis(2));
                            continue;
                        }
                    };
                    if let Err(e) = host.process(&mut frame) {
                        warn!(error = %e, "denoiser error; passing frame through");
                    }
                    if prod_b.try_push(frame).is_err() {
                        // Render is behind — drop the oldest by popping.
                        // Better than blocking the DSP thread.
                        warn!("ring B full; dropping a frame");
                    }
                }
            })
            .context("spawn dsp thread")?;

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
        }
        impl<C: Consumer<Item = Frame> + Send> audio_io::wasapi_render::FrameSource for Source<C> {
            fn next_frame(&mut self) -> Option<Frame> {
                self.cons.try_pop()
            }
            fn on_underrun(&mut self) {
                tracing::warn!("render underrun");
            }
        }

        let render = audio_io::WasapiRender::start(
            &output_id,
            Box::new(Source { cons: cons_b }),
        )
        .map_err(|e| anyhow::anyhow!(e))?;

        Ok(Self {
            capture,
            render,
            dsp_thread: Some(dsp_thread),
            bypass,
            stats,
            denoiser_name,
            shutdown,
        })
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

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(t) = self.dsp_thread.take() {
            let _ = t.join();
        }
    }
}
