//! NoiseGate — real-time mic noise cancellation for Windows.
//!
//! Architecture:
//!   physical mic ─► WASAPI capture ─► ring buffer A ─► DSP (DeepFilterNet3)
//!                                                               │
//!                                                               ▼
//!                                                       ring buffer B
//!                                                               │
//!                                              WASAPI render ──┘──► VB-Cable Input
//!
//! The tray UI lives on the main thread. Audio runs on three dedicated
//! MMCSS "Pro Audio" threads spawned by the audio-io and pipeline modules.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod pipeline;
#[cfg(windows)]
mod tray;

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::info;

fn init_tracing() {
    let log_dir = config::log_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file = log_dir.join("noisegate.log");

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .ok();

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,noisegate=debug"));

    use tracing_subscriber::fmt;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let stdout_layer = fmt::layer().with_target(false);
    let registry = tracing_subscriber::registry().with(env_filter).with(stdout_layer);

    if let Some(file) = file {
        registry
            .with(fmt::layer().with_writer(file).with_ansi(false).with_target(false))
            .init();
    } else {
        registry.init();
    }
}

#[cfg(windows)]
fn main() -> Result<()> {
    init_tracing();

    // Single-instance lock via a named mutex. Prevents two trays from
    // fighting over the same audio devices.
    let _lock = single_instance::acquire()
        .context("another NoiseGate instance is already running")?;

    info!("NoiseGate starting");

    let cfg = Arc::new(parking_lot_compat::RwLock::new(config::Config::load_or_default()));
    let pipeline = pipeline::Pipeline::start(cfg.clone())?;
    info!(
        denoiser = pipeline.denoiser_name(),
        "audio pipeline running"
    );

    tray::run(cfg, pipeline)?;
    info!("NoiseGate exiting");
    Ok(())
}

#[cfg(not(windows))]
fn main() -> Result<()> {
    init_tracing();
    anyhow::bail!("NoiseGate is Windows-only. Build for x86_64-pc-windows-msvc.");
}

#[cfg(windows)]
mod single_instance {
    use anyhow::{anyhow, Result};
    use windows::core::w;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows::Win32::System::Threading::CreateMutexW;

    pub struct Lock(HANDLE);

    impl Drop for Lock {
        fn drop(&mut self) {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }

    pub fn acquire() -> Result<Lock> {
        unsafe {
            let h = CreateMutexW(None, true, w!("Global\\NoiseGate.SingleInstance"))
                .map_err(|e| anyhow!("CreateMutexW: {e}"))?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                return Err(anyhow!("already running"));
            }
            Ok(Lock(h))
        }
    }
}

/// Lightweight RwLock shim — we don't pull in parking_lot for a single
/// usage. std's RwLock is fine for config reads.
mod parking_lot_compat {
    pub use std::sync::RwLock;
}
