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

// GUI subsystem: no console window when started normally (double-click,
// autostart). When launched from a terminal we attach to that terminal so
// `--list-devices`, `--help` and the live log still print there, and
// `--console` opens a log window on demand. See `mod console` below.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod autostart;
mod banner;
mod config;
mod log_format;
#[cfg(windows)]
mod panel;
mod pipeline;
#[cfg(windows)]
mod theme;
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

    // Stdout: green-themed, custom format; ANSI disabled in the layer
    // because GreenFormat writes its own escape codes.
    let stdout_layer = fmt::layer()
        .with_ansi(false)
        .event_format(log_format::GreenFormat::new());

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer);

    // File: plain, no colors, default format. Useful for sharing logs.
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
    let console = console::setup(std::env::args().any(|a| a == "--console"));
    let res = run();
    if let Err(e) = &res {
        // Without a console nobody would see the error: show it.
        if !console {
            console::error_box(&format!("{e:#}"));
        }
    }
    res
}

#[cfg(windows)]
fn run() -> Result<()> {
    let args = parse_args();
    if args.help {
        banner::print();
        print_help();
        return Ok(());
    }

    if args.list_devices {
        banner::print();
        return list_devices();
    }

    // Hidden: render the tray icons for previewing (see tray::dump_icons).
    if let Some(dir) = args.dump_icons.as_deref() {
        return tray::dump_icons(std::path::Path::new(dir));
    }

    banner::print();
    init_tracing();

    // Single-instance lock via a named mutex. Prevents two trays from
    // fighting over the same audio devices.
    let _lock = single_instance::acquire()
        .context("another NoiseGate instance is already running")?;

    info!("NoiseGate starting");

    // Always print the device inventory at startup so users can identify
    // which mic to pick — especially important to spot Bluetooth-HFP
    // endpoints (which sound terrible) vs USB/wired mics.
    log_input_devices();

    let mut cfg_value = config::Config::load_or_default();
    // Apply CLI overrides (don't persist them — that's what the tray menu
    // is for once it's wired up).
    if let Some(out_filter) = args.out.as_deref() {
        match resolve_device_by_substring(out_filter, audio_io::DeviceDirection::Render) {
            Ok(id) => {
                info!(filter = out_filter, resolved = %id, "--out override applied");
                cfg_value.output_device_id = id;
            }
            Err(e) => {
                anyhow::bail!("--out '{}' did not match any output device: {e}", out_filter);
            }
        }
    }
    if args.bypass {
        cfg_value.enabled = false;
        info!("--bypass: passing audio through without denoising");
    }
    if let Some(mic_filter) = args.mic.as_deref() {
        match resolve_device_by_substring(mic_filter, audio_io::DeviceDirection::Capture) {
            Ok(id) => {
                info!(filter = mic_filter, resolved = %id, "--mic override applied");
                cfg_value.input_device_id = id;
            }
            Err(e) => {
                anyhow::bail!("--mic '{}' did not match any input device: {e}", mic_filter);
            }
        }
    }

    let cfg = Arc::new(parking_lot_compat::RwLock::new(cfg_value));
    let pipeline = pipeline::Pipeline::start(cfg.clone())?;
    info!(
        denoiser = pipeline.denoiser_name(),
        "audio pipeline running"
    );

    tray::run(cfg, pipeline)?;
    info!("NoiseGate exiting");
    Ok(())
}

#[derive(Default)]
struct CliArgs {
    help: bool,
    list_devices: bool,
    mic: Option<String>,
    out: Option<String>,
    bypass: bool,
    dump_icons: Option<String>,
}

fn parse_args() -> CliArgs {
    let mut out = CliArgs::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => out.help = true,
            "--list-devices" => out.list_devices = true,
            "--mic" => out.mic = args.next(),
            other if other.starts_with("--mic=") => {
                out.mic = Some(other["--mic=".len()..].to_string());
            }
            "--out" => out.out = args.next(),
            other if other.starts_with("--out=") => {
                out.out = Some(other["--out=".len()..].to_string());
            }
            "--bypass" => out.bypass = true,
            "--dump-icons" => out.dump_icons = args.next(),
            "--console" => {} // handled in main() before anything prints
            _ => {} // ignore unknowns silently
        }
    }
    out
}

fn print_help() {
    println!(
        "NoiseGate — real-time mic noise cancellation\n\
         \n\
         USAGE:\n\
             noisegate.exe [OPTIONS]\n\
         \n\
         OPTIONS:\n\
             --list-devices         Print all input/output devices and exit.\n\
             --mic <SUBSTRING>      Pick the input device whose friendly name contains\n\
                                    SUBSTRING (case-insensitive). Useful when your\n\
                                    default mic is a Bluetooth headset and you want\n\
                                    a USB mic instead.\n\
             --out <SUBSTRING>      Pick the output device for the cleaned audio.\n\
                                    Default: the first virtual cable found\n\
                                    (VB-Cable, VoiceMeeter, Steam Streaming Microphone).\n\
             --bypass               Start with denoising off (pass-through).\n\
             --console              Open a window with the live log.\n\
             -h, --help             Show this help.\n\
         \n\
         CONFIG FILE:\n\
             %APPDATA%\\NoiseGate\\config.toml — `input_device_id` / `output_device_id`\n\
             can be set there for a permanent choice.\n"
    );
}

#[cfg(windows)]
fn list_devices() -> Result<()> {
    use audio_io::devices::DeviceList;
    let list = DeviceList::enumerate().context("enumerating devices")?;
    println!("Capture (input) devices:");
    for d in &list.capture {
        let tag = if d.is_default { " [default]" } else { "" };
        println!("  - {}{}", d.friendly_name, tag);
        println!("    id: {}", d.id);
    }
    println!("\nRender (output) devices:");
    for d in &list.render {
        let tag = if d.is_default { " [default]" } else { "" };
        match d.virtual_cable() {
            Some(c) => println!(
                "  - {}{}  [virtual cable: other apps use \"{}\"]",
                d.friendly_name, tag, c.mic_name
            ),
            None => println!("  - {}{}", d.friendly_name, tag),
        }
        println!("    id: {}", d.id);
    }
    Ok(())
}

#[cfg(not(windows))]
fn list_devices() -> Result<()> {
    anyhow::bail!("--list-devices only works on Windows.")
}

#[cfg(windows)]
fn log_input_devices() {
    use audio_io::devices::DeviceList;
    let Ok(list) = DeviceList::enumerate() else { return };
    for d in &list.capture {
        let default = if d.is_default { " (default)" } else { "" };
        info!(name = %d.friendly_name, id = %d.id, "input device available{}", default);
    }
}

#[cfg(windows)]
fn resolve_device_by_substring(
    needle: &str,
    direction: audio_io::DeviceDirection,
) -> Result<String> {
    use audio_io::devices::DeviceList;
    let needle_lc = needle.to_ascii_lowercase();
    let list = DeviceList::enumerate()?;
    let pool = match direction {
        audio_io::DeviceDirection::Capture => &list.capture,
        audio_io::DeviceDirection::Render => &list.render,
    };
    let matches: Vec<_> = pool
        .iter()
        .filter(|d| d.friendly_name.to_ascii_lowercase().contains(&needle_lc))
        .collect();
    match matches.as_slice() {
        [] => Err(anyhow::anyhow!("no capture device matched")),
        [d] => Ok(d.id.clone()),
        many => {
            let names: Vec<_> = many.iter().map(|d| d.friendly_name.as_str()).collect();
            Err(anyhow::anyhow!(
                "ambiguous: {} devices matched ({}). Refine the substring.",
                many.len(),
                names.join(", ")
            ))
        }
    }
}

#[cfg(not(windows))]
fn main() -> Result<()> {
    init_tracing();
    anyhow::bail!("NoiseGate is Windows-only. Build for x86_64-pc-windows-msvc.");
}

#[cfg(windows)]
mod console {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Console::{
        AllocConsole, AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    /// Give stdout/stderr somewhere to go. Returns true if output is visible
    /// somewhere (a redirect, the parent terminal, or a new console window).
    pub fn setup(force_window: bool) -> bool {
        unsafe {
            // Already redirected (pipe/file, e.g. `> log.txt`): keep it.
            if let Ok(h) = GetStdHandle(STD_OUTPUT_HANDLE) {
                if !h.is_invalid() && !h.0.is_null() {
                    return true;
                }
            }
            if force_window {
                return AllocConsole().is_ok();
            }
            // Started from a terminal: print into it. Started from Explorer:
            // there's no parent console and we stay windowless.
            AttachConsole(ATTACH_PARENT_PROCESS).is_ok()
        }
    }

    pub fn error_box(text: &str) {
        unsafe {
            MessageBoxW(
                HWND::default(),
                &HSTRING::from(text),
                &HSTRING::from("NoiseGate"),
                MB_OK | MB_ICONERROR,
            );
        }
    }
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
            // Wrap first so Drop closes the handle on the error path too.
            let lock = Lock(h);
            if GetLastError() == ERROR_ALREADY_EXISTS {
                return Err(anyhow!("already running"));
            }
            Ok(lock)
        }
    }
}

/// Lightweight RwLock shim — we don't pull in parking_lot for a single
/// usage. std's RwLock is fine for config reads.
mod parking_lot_compat {
    pub use std::sync::RwLock;
}
