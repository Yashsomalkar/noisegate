//! System tray UI: enable/disable toggle, device picker, CPU meter, quit.
//!
//! `tray-icon` needs a window-message pump on the main thread, so we drive
//! it with a `winit` event loop (no actual window — just the tray icon).

#![cfg(windows)]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{error, info};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event_loop::{ControlFlow, EventLoop};

use crate::config::Config;
use crate::parking_lot_compat::RwLock;
use crate::pipeline::Pipeline;

/// DeepFilterNet3 suppression presets: (menu label, attenuation limit dB).
const STRENGTHS: &[(&str, f32)] = &[
    ("Maximum", 100.0),
    ("Strong (24 dB)", 24.0),
    ("Natural (12 dB)", 12.0),
];

pub fn run(cfg: Arc<RwLock<Config>>, pipeline: Pipeline) -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    // Forward tray events to winit so we run on a single event loop.
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));

    let mut app = App {
        cfg,
        pipeline: Some(pipeline),
        last_error: None,
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
    /// None only while a restart failed (e.g. the picked mic vanished).
    pipeline: Option<Pipeline>,
    last_error: Option<String>,
    tray: Option<TrayIcon>,
    items: Option<Items>,
    last_tooltip_update: Instant,
}

struct Items {
    enable: CheckMenuItem,
    /// Microphone submenu; rebuilt by "Refresh devices".
    mic_menu: Submenu,
    /// (item, capture device id — empty = system default)
    mics: Vec<(CheckMenuItem, String)>,
    refresh: MenuItem,
    /// (item, backend key)
    models: Vec<(CheckMenuItem, &'static str)>,
    /// (item, attenuation limit dB)
    strengths: Vec<(CheckMenuItem, f32)>,
    open_logs: MenuItem,
    quit: MenuItem,
}

/// Check exactly the item whose id is `id` (radio-group behaviour).
fn check_only<T>(group: &[(CheckMenuItem, T)], id: &MenuId) {
    for (item, _) in group {
        item.set_checked(item.id() == id);
    }
}

/// Real microphones only: the mic side of a virtual cable is where *our*
/// output goes, so offering it as an input would just loop.
fn list_mics() -> Vec<(String, String)> {
    let Ok(list) = audio_io::DeviceList::enumerate() else { return Vec::new() };
    list.capture
        .into_iter()
        .filter(|d| {
            !audio_io::devices::VIRTUAL_CABLES
                .iter()
                .any(|c| c.matches_name(&d.friendly_name))
        })
        .map(|d| (d.id, d.friendly_name))
        .collect()
}

fn fill_mic_menu(menu: &Submenu, selected: &str) -> Vec<(CheckMenuItem, String)> {
    let mut out = vec![(
        CheckMenuItem::new("System default", true, selected.is_empty(), None),
        String::new(),
    )];
    for (id, name) in list_mics() {
        let checked = id == selected;
        out.push((CheckMenuItem::new(name, true, checked, None), id));
    }
    for (item, _) in &out {
        menu.append(item).ok();
    }
    out
}

impl App {
    /// Stop the running pipeline and start a new one from the current config.
    fn restart_pipeline(&mut self) {
        // Drop first: joins the capture/render/DSP threads.
        self.pipeline = None;
        match Pipeline::start(self.cfg.clone()) {
            Ok(p) => {
                info!(denoiser = p.denoiser_name(), "audio pipeline restarted");
                self.pipeline = Some(p);
                self.last_error = None;
            }
            Err(e) => {
                error!(error = %format!("{e:#}"), "restarting audio pipeline failed");
                self.last_error = Some(format!("{e:#}"));
            }
        }
        self.refresh_tooltip();
    }

    fn refresh_tooltip(&self) {
        if let Some(tray) = &self.tray {
            let text = match (&self.pipeline, &self.last_error) {
                (Some(p), _) => tooltip(p),
                (None, Some(e)) => format!("NoiseGate — stopped: {e}"),
                (None, None) => "NoiseGate — stopped".into(),
            };
            // Windows truncates tray tooltips at 127 chars.
            let _ = tray.set_tooltip(Some(text.chars().take(127).collect::<String>()));
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        if self.tray.is_some() {
            return;
        }
        let cfg = self.cfg.read().unwrap().clone();
        let enabled = self.pipeline.as_ref().map(|p| p.is_enabled()).unwrap_or(cfg.enabled);

        let menu = Menu::new();
        let enable = CheckMenuItem::new("Enabled", true, enabled, None);

        let mic_menu = Submenu::new("Microphone", true);
        let mics = fill_mic_menu(&mic_menu, &cfg.input_device_id);
        let refresh = MenuItem::new("Refresh devices", true, None);

        let model_menu = Submenu::new("Noise suppression", true);
        let current_key = self
            .pipeline
            .as_ref()
            .and_then(|p| {
                // Labels and runtime names share their first word
                // ("DeepFilterNet3 …", "RNNoise …").
                let first = |s: &str| s.split(' ').next().unwrap_or("").to_string();
                dsp::BACKENDS.iter().find(|b| first(b.label) == first(p.denoiser_name()))
            })
            .map(|b| b.key)
            .unwrap_or("");
        let models: Vec<_> = dsp::BACKENDS
            .iter()
            .map(|b| (CheckMenuItem::new(b.label, true, b.key == current_key, None), b.key))
            .collect();
        for (item, _) in &models {
            model_menu.append(item).ok();
        }
        model_menu.append(&PredefinedMenuItem::separator()).ok();
        let strength_menu = Submenu::new("Strength (DeepFilterNet3)", true);
        let strengths: Vec<_> = STRENGTHS
            .iter()
            .map(|&(label, db)| {
                let checked = (cfg.attenuation_db - db).abs() < 0.5;
                (CheckMenuItem::new(label, true, checked, None), db)
            })
            .collect();
        for (item, _) in &strengths {
            strength_menu.append(item).ok();
        }
        model_menu.append(&strength_menu).ok();

        let open_logs = MenuItem::new("Open log folder", true, None);
        let quit = MenuItem::new("Quit NoiseGate", true, None);

        menu.append(&enable).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&mic_menu).ok();
        menu.append(&model_menu).ok();
        menu.append(&refresh).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&open_logs).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&quit).ok();

        let icon = build_default_icon();
        let initial = match &self.pipeline {
            Some(p) => initial_tooltip(p),
            None => "NoiseGate — stopped".into(),
        };
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(initial)
            .with_icon(icon)
            .build()
            .expect("build tray icon");

        self.tray = Some(tray);
        self.items = Some(Items {
            enable,
            mic_menu,
            mics,
            refresh,
            models,
            strengths,
            open_logs,
            quit,
        });

        // Tick periodically so we can refresh the tooltip CPU meter.
        event_loop.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(500)));
    }

    fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, ev: UserEvent) {
        let UserEvent::Menu(MenuEvent { id, .. }) = ev;
        let Some(items) = &mut self.items else { return };

        // Device / model changes need a pipeline restart, done after the
        // config write lock is released.
        let mut restart = false;

        if id == items.enable.id() {
            let now_enabled = items.enable.is_checked();
            if let Some(p) = &self.pipeline {
                p.set_enabled(now_enabled);
            }
            let mut c = self.cfg.write().unwrap();
            c.enabled = now_enabled;
            let _ = c.save();
            info!(enabled = now_enabled, "user toggled enable");
        } else if let Some((_, dev_id)) = items.mics.iter().find(|(i, _)| i.id() == &id) {
            let dev_id = dev_id.clone();
            check_only(&items.mics, &id);
            let mut c = self.cfg.write().unwrap();
            if c.input_device_id != dev_id {
                c.input_device_id = dev_id;
                let _ = c.save();
                info!(input = %c.input_device_id, "user picked microphone");
                restart = true;
            }
        } else if id == items.refresh.id() {
            for (item, _) in items.mics.drain(..) {
                items.mic_menu.remove(&item).ok();
            }
            let selected = self.cfg.read().unwrap().input_device_id.clone();
            items.mics = fill_mic_menu(&items.mic_menu, &selected);
            info!(count = items.mics.len() - 1, "microphone list refreshed");
        } else if let Some(&(_, key)) = items.models.iter().find(|(i, _)| i.id() == &id) {
            check_only(&items.models, &id);
            let mut c = self.cfg.write().unwrap();
            if c.denoiser != key {
                c.denoiser = key.to_string();
                let _ = c.save();
                info!(denoiser = key, "user picked denoiser");
                restart = true;
            }
        } else if let Some(&(_, db)) = items.strengths.iter().find(|(i, _)| i.id() == &id) {
            check_only(&items.strengths, &id);
            let mut c = self.cfg.write().unwrap();
            if (c.attenuation_db - db).abs() > 0.01 {
                c.attenuation_db = db;
                let _ = c.save();
                info!(attenuation_db = db, "user picked suppression strength");
                // Only DeepFilterNet3 uses it; restart only if it's running.
                restart = c.denoiser.is_empty() || c.denoiser == "deepfilter";
            }
        } else if id == items.open_logs.id() {
            let _ = std::process::Command::new("explorer")
                .arg(crate::config::log_dir())
                .spawn();
        } else if id == items.quit.id() {
            info!("quit requested");
            event_loop.exit();
        }

        if restart {
            self.restart_pipeline();
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
            self.refresh_tooltip();
        }
    }
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
                rgba[i] = 0x2a;     // R
                rgba[i + 1] = 0xa1; // G
                rgba[i + 2] = 0x98; // B
                rgba[i + 3] = 0xff;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, 16, 16).expect("valid icon")
}
