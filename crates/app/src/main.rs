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

// During pre-alpha we run as a console subsystem so users can see live
// logs and run CLI flags like `--list-devices` / `--mic` from PowerShell.
// Switch to `windows_subsystem = "windows"` once the app is stable enough
// to justify hiding the console window.

mod banner;
mod config;
mod log_format;
mod offline;
mod pipeline;
#[cfg(windows)]
mod tray;

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::info;

/// Roll the log over once it gets big. It's an append-only file that records
/// every device name we see on every run, so left alone it grows forever and
/// accumulates more of the user's hardware inventory than anyone expects to
/// hand over when they paste a log into a bug report.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

fn rotate_log_if_large(log_file: &std::path::Path, max_bytes: u64) {
    let too_big = std::fs::metadata(log_file)
        .map(|m| m.len() > max_bytes)
        .unwrap_or(false);
    if too_big {
        let _ = std::fs::rename(log_file, log_file.with_extension("log.old"));
    }
}

fn init_tracing() {
    let log_dir = config::log_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file = log_dir.join("noisegate.log");
    rotate_log_if_large(&log_file, MAX_LOG_BYTES);

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

    banner::print();
    init_tracing();

    let model = args.model.as_deref().map(std::path::Path::new);
    if let Some((input, output)) = &args.denoise {
        return offline::denoise_file(
            std::path::Path::new(input),
            std::path::Path::new(output),
            model,
        );
    }
    if let Some((seconds, path)) = &args.record {
        let device = match args.mic.as_deref() {
            Some(filter) => resolve_mic_by_substring(filter)?,
            None => String::new(),
        };
        return offline::record(&device, *seconds, std::path::Path::new(path));
    }

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
    if let Some(mic_filter) = args.mic.as_deref() {
        match resolve_mic_by_substring(mic_filter) {
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
    /// `--record <SECONDS> <FILE.wav>`
    record: Option<(f32, String)>,
    /// `--denoise <IN.wav> <OUT.wav>`
    denoise: Option<(String, String)>,
    /// `--model <FILE.onnx>` — overrides config for the offline tools.
    model: Option<String>,
}

fn parse_args() -> CliArgs {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from<I, S>(args: I) -> CliArgs
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut out = CliArgs::default();
    let mut args = args.into_iter().map(Into::into);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => out.help = true,
            "--list-devices" => out.list_devices = true,
            "--mic" => out.mic = args.next(),
            other if other.starts_with("--mic=") => {
                out.mic = Some(other["--mic=".len()..].to_string());
            }
            "--model" => out.model = args.next(),
            "--record" => {
                if let (Some(secs), Some(path)) = (args.next(), args.next()) {
                    out.record = secs.parse().ok().map(|s| (s, path));
                }
            }
            "--denoise" => {
                if let (Some(input), Some(output)) = (args.next(), args.next()) {
                    out.denoise = Some((input, output));
                }
            }
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
        let vb = if d.is_vb_cable_input() { "  [VB-Cable]" } else { "" };
        println!("  - {}{}{}", d.friendly_name, tag, vb);
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
fn resolve_mic_by_substring(needle: &str) -> Result<String> {
    use audio_io::devices::DeviceList;
    let needle_lc = needle.to_ascii_lowercase();
    let list = DeviceList::enumerate()?;
    let matches: Vec<_> = list
        .capture
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

    /// The name is deliberately session-local (`Local\`, not `Global\`): a
    /// `Global\` name is visible to every session on the machine, so any
    /// other user or process could squat it and permanently block startup —
    /// and creating one needs SeCreateGlobalPrivilege, which a standard user
    /// account doesn't have. One tray per login session is what we actually
    /// want anyway.
    pub fn acquire() -> Result<Lock> {
        unsafe {
            let h = CreateMutexW(None, true, w!("Local\\NoiseGate.SingleInstance"))
                .map_err(|e| anyhow!("CreateMutexW: {e}"))?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                // We own this handle even when the mutex already existed.
                let _ = windows::Win32::Foundation::CloseHandle(h);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("noisegate-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn small_logs_are_left_alone() {
        let dir = scratch_dir("small-log");
        let log = dir.join("noisegate.log");
        std::fs::write(&log, b"a few lines of startup chatter").unwrap();

        rotate_log_if_large(&log, 1024);

        assert!(log.exists(), "small log should not be rotated");
        assert!(!log.with_extension("log.old").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_logs_are_rolled_over() {
        let dir = scratch_dir("big-log");
        let log = dir.join("noisegate.log");
        std::fs::write(&log, vec![b'x'; 2048]).unwrap();

        rotate_log_if_large(&log, 1024);

        assert!(!log.exists(), "oversized log should have been moved aside");
        let rolled = log.with_extension("log.old");
        assert!(rolled.exists(), "expected noisegate.log.old");
        assert_eq!(std::fs::metadata(&rolled).unwrap().len(), 2048);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_log_is_not_an_error() {
        let dir = scratch_dir("no-log");
        // First run: nothing to rotate, and nothing should blow up.
        rotate_log_if_large(&dir.join("noisegate.log"), 1024);
        assert!(!dir.join("noisegate.log.old").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_parses_mic_in_both_spellings() {
        // `--mic X` and `--mic=X` must resolve identically, and unknown flags
        // stay ignored rather than being swallowed as a device name.
        let split = parse_args_from(["--mic", "Yeti"]);
        let joined = parse_args_from(["--mic=Yeti"]);
        assert_eq!(split.mic.as_deref(), Some("Yeti"));
        assert_eq!(joined.mic.as_deref(), Some("Yeti"));

        let listed = parse_args_from(["--list-devices", "--nonsense"]);
        assert!(listed.list_devices);
        assert!(listed.mic.is_none());
    }
}
