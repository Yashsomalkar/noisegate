//! System tray UI: enable/disable toggle, microphone picker, start-at-login,
//! CPU meter, log folder, quit.
//!
//! `tray-icon` needs a window-message pump on the main thread, so we drive
//! it with a `winit` event loop (no actual window — just the tray icon).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{info, warn};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event_loop::{ControlFlow, EventLoop};

use audio_io::devices::DeviceList;

use crate::config::Config;
use crate::parking_lot_compat::RwLock;
use crate::pipeline::Pipeline;

pub fn run(cfg: Arc<RwLock<Config>>, pipeline: Option<Pipeline>) -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    // Forward tray events to winit so we run on a single event loop.
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));

    let mut app = App {
        cfg,
        pipeline,
        tray: None,
        items: None,
        last_tooltip_update: Instant::now(),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
}

struct App {
    cfg: Arc<RwLock<Config>>,
    /// `None` only while a device switch is restarting it, or if a restart
    /// failed — the tray stays alive either way so the user can pick again.
    pipeline: Option<Pipeline>,
    tray: Option<TrayIcon>,
    items: Option<Items>,
    last_tooltip_update: Instant,
}

/// One entry in the microphone submenu.
struct MicEntry {
    item: CheckMenuItem,
    /// Empty string = follow the Windows default device.
    device_id: String,
}

struct Items {
    enable: CheckMenuItem,
    mics: Vec<MicEntry>,
    auto_start: CheckMenuItem,
    open_logs: MenuItem,
    quit: MenuItem,
}

impl Items {
    fn mic_by_id(&self, id: &MenuId) -> Option<&MicEntry> {
        self.mics.iter().find(|m| m.item.id() == id)
    }
}

/// Label for a capture device in the picker. The Windows default entry is
/// listed first and selected when no explicit device is configured, so the
/// out-of-the-box behaviour matches what the rest of the system does.
fn mic_label(name: &str, is_system_default: bool) -> String {
    if is_system_default {
        format!("{name}  (system default)")
    } else {
        name.to_string()
    }
}

fn build_menu(cfg: &Config) -> (Menu, Items) {
    let menu = Menu::new();

    let enable = CheckMenuItem::new("Enabled", true, cfg.enabled, None);
    menu.append(&enable).ok();
    menu.append(&PredefinedMenuItem::separator()).ok();

    // Microphone picker.
    let mic_menu = Submenu::new("Microphone", true);
    let mut mics = Vec::new();

    let follow_default = cfg.input_device_id.is_empty();
    let default_item = CheckMenuItem::new("Windows default", true, follow_default, None);
    mic_menu.append(&default_item).ok();
    mics.push(MicEntry {
        item: default_item,
        device_id: String::new(),
    });

    match DeviceList::enumerate() {
        Ok(list) => {
            if !list.capture.is_empty() {
                mic_menu.append(&PredefinedMenuItem::separator()).ok();
            }
            for d in &list.capture {
                let checked = !follow_default && d.id == cfg.input_device_id;
                let item = CheckMenuItem::new(mic_label(&d.friendly_name, d.is_default), true, checked, None);
                mic_menu.append(&item).ok();
                mics.push(MicEntry {
                    item,
                    device_id: d.id.clone(),
                });
            }
        }
        Err(e) => {
            warn!(error = %e, "could not enumerate capture devices for the tray menu");
            let item = MenuItem::new("(no microphones found)", false, None);
            mic_menu.append(&item).ok();
        }
    }
    menu.append(&mic_menu).ok();
    menu.append(&PredefinedMenuItem::separator()).ok();

    // Ask the registry rather than trusting the config file: the user may have
    // removed the Run entry by hand since we last wrote it.
    let auto_start = CheckMenuItem::new("Start with Windows", true, crate::autostart::is_enabled(), None);
    menu.append(&auto_start).ok();

    let open_logs = MenuItem::new("Open log folder", true, None);
    menu.append(&open_logs).ok();
    menu.append(&PredefinedMenuItem::separator()).ok();

    let quit = MenuItem::new("Quit NoiseGate", true, None);
    menu.append(&quit).ok();

    (
        menu,
        Items {
            enable,
            mics,
            auto_start,
            open_logs,
            quit,
        },
    )
}

impl App {
    /// Restart the audio pipeline against the current config. The old one is
    /// dropped first so it releases the capture device before we reopen it.
    fn restart_pipeline(&mut self) {
        drop(self.pipeline.take());
        match Pipeline::start(self.cfg.clone()) {
            Ok(p) => {
                info!(denoiser = p.denoiser_name(), "pipeline restarted");
                self.pipeline = Some(p);
            }
            Err(e) => {
                // Leave the tray running: the user picked a bad device and the
                // fix is to pick a different one from this very menu.
                warn!(error = %e, "restarting the pipeline failed");
                crate::message_box(&format!("Could not start audio with that device:\n\n{e:#}"));
            }
        }
    }

    fn select_mic(&mut self, device_id: String) {
        {
            let mut c = self.cfg.write().unwrap();
            c.input_device_id = device_id.clone();
            if let Err(e) = c.save() {
                warn!(error = %e, "saving config failed");
            }
        }
        // Exactly one entry stays checked — these are radio buttons wearing
        // checkbox clothing, and muda won't enforce that for us.
        if let Some(items) = &self.items {
            for m in &items.mics {
                m.item.set_checked(m.device_id == device_id);
            }
        }
        info!(device = %if device_id.is_empty() { "Windows default" } else { &device_id }, "microphone selected");
        self.restart_pipeline();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        if self.tray.is_some() {
            return;
        }
        let (menu, items) = build_menu(&self.cfg.read().unwrap());

        let tooltip = self
            .pipeline
            .as_ref()
            .map(initial_tooltip)
            .unwrap_or_else(|| "NoiseGate — stopped".to_string());

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip)
            .with_icon(build_default_icon())
            .build()
            .expect("build tray icon");

        self.tray = Some(tray);
        self.items = Some(items);

        // Tick periodically so we can refresh the tooltip CPU meter.
        event_loop.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(500)));
    }

    fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, ev: UserEvent) {
        let UserEvent::Menu(MenuEvent { id, .. }) = ev;
        let Some(items) = &self.items else { return };

        if id == *items.enable.id() {
            let now_enabled = items.enable.is_checked();
            if let Some(p) = &self.pipeline {
                p.set_enabled(now_enabled);
            }
            let mut c = self.cfg.write().unwrap();
            c.enabled = now_enabled;
            let _ = c.save();
            info!(enabled = now_enabled, "user toggled enable");
        } else if id == *items.auto_start.id() {
            let wanted = items.auto_start.is_checked();
            match crate::autostart::set(wanted) {
                Ok(()) => {
                    let mut c = self.cfg.write().unwrap();
                    c.auto_start = wanted;
                    let _ = c.save();
                    info!(auto_start = wanted, "start-with-Windows toggled");
                }
                Err(e) => {
                    warn!(error = %e, "could not update the Run key");
                    // Put the checkbox back where it was — it must reflect the
                    // registry, not what the user wished for.
                    items.auto_start.set_checked(!wanted);
                    crate::message_box(&format!("Could not change start-with-Windows:\n\n{e:#}"));
                }
            }
        } else if id == *items.open_logs.id() {
            let _ = std::process::Command::new(explorer_path())
                .arg(crate::config::log_dir())
                .spawn();
        } else if id == *items.quit.id() {
            info!("quit requested");
            event_loop.exit();
        } else if let Some(device_id) = items.mic_by_id(&id).map(|m| m.device_id.clone()) {
            self.select_mic(device_id);
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &winit::event_loop::ActiveEventLoop,
        _id: winit::window::WindowId,
        _event: winit::event::WindowEvent,
    ) {
        // No window — nothing to do.
    }

    fn about_to_wait(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {
        if self.last_tooltip_update.elapsed() >= Duration::from_millis(1000) {
            self.last_tooltip_update = Instant::now();
            if let Some(tray) = &self.tray {
                let text = match &self.pipeline {
                    Some(p) => tooltip(p),
                    None => "NoiseGate — stopped (pick a microphone)".to_string(),
                };
                let _ = tray.set_tooltip(Some(text));
            }
        }
    }
}

/// Absolute path to Explorer. Spawning bare `"explorer"` would resolve it
/// through PATH, so any writable directory sitting earlier on PATH gets to
/// supply the binary we launch.
fn explorer_path() -> std::path::PathBuf {
    std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
        .join("explorer.exe")
}

fn initial_tooltip(p: &Pipeline) -> String {
    format!("NoiseGate ({}) — starting", p.denoiser_name())
}

fn tooltip(p: &Pipeline) -> String {
    let s = p.stats();
    let frames = s.frames.load(Ordering::Relaxed);
    let total_ns = s.dsp_ns.load(Ordering::Relaxed);
    let peak_ns = s.peak_frame_ns.load(Ordering::Relaxed);
    // Each frame represents 10 ms of audio. CPU% = total_dsp_time / wallclock_audio_time.
    let cpu_pct = if frames == 0 {
        0.0
    } else {
        let avg_dsp_ms = (total_ns as f64 / frames as f64) / 1_000_000.0;
        avg_dsp_ms / 10.0 * 100.0
    };
    format!(
        "NoiseGate ({})\n{}  |  CPU: {:.1}%  peak: {:.1}ms",
        p.denoiser_name(),
        if p.is_enabled() { "ON" } else { "BYPASS" },
        cpu_pct,
        peak_ns as f64 / 1_000_000.0,
    )
}

fn build_default_icon() -> tray_icon::Icon {
    // Generate a simple 16x16 RGBA icon procedurally so we don't need to
    // ship a .ico in v1. Replace with a real icon later.
    let mut rgba = vec![0u8; 16 * 16 * 4];
    for y in 0..16 {
        for x in 0..16 {
            let i = (y * 16 + x) * 4;
            let on_circle = ((x as i32 - 8).pow(2) + (y as i32 - 8).pow(2)) <= 49;
            if on_circle {
                rgba[i] = 0x2a; // R
                rgba[i + 1] = 0xa1; // G
                rgba[i + 2] = 0x98; // B
                rgba[i + 3] = 0xff;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, 16, 16).expect("valid icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_is_resolved_absolutely_and_exists() {
        let p = explorer_path();
        assert!(p.is_absolute(), "must not be resolved through PATH");
        assert_eq!(p.file_name().unwrap(), "explorer.exe");
        assert!(p.exists(), "expected the real Explorer at {}", p.display());
    }

    #[test]
    fn system_default_mic_is_marked_in_the_label() {
        assert_eq!(mic_label("Yeti", false), "Yeti");
        assert!(mic_label("Yeti", true).starts_with("Yeti"));
        assert!(mic_label("Yeti", true).contains("system default"));
    }
}
