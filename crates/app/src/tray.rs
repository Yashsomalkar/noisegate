//! System tray UI: enable/disable toggle, device picker, CPU meter, quit.
//!
//! `tray-icon` needs a window-message pump on the main thread, so we drive
//! it with a `winit` event loop (no actual window — just the tray icon).

#![cfg(windows)]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::info;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy};

use crate::config::Config;
use crate::parking_lot_compat::RwLock;
use crate::pipeline::Pipeline;

pub fn run(cfg: Arc<RwLock<Config>>, pipeline: Pipeline) -> Result<()> {
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
    pipeline: Pipeline,
    tray: Option<TrayIcon>,
    items: Option<Items>,
    last_tooltip_update: Instant,
}

struct Items {
    enable: CheckMenuItem,
    open_logs: MenuItem,
    quit: MenuItem,
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        if self.tray.is_some() {
            return;
        }
        let menu = Menu::new();
        let enable = CheckMenuItem::new("Enabled", true, self.pipeline.is_enabled(), None);
        let open_logs = MenuItem::new("Open log folder", true, None);
        let quit = MenuItem::new("Quit NoiseGate", true, None);

        menu.append(&enable).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&open_logs).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&quit).ok();

        let icon = build_default_icon();
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(initial_tooltip(&self.pipeline))
            .with_icon(icon)
            .build()
            .expect("build tray icon");

        self.tray = Some(tray);
        self.items = Some(Items { enable, open_logs, quit });

        // Tick periodically so we can refresh the tooltip CPU meter.
        event_loop.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(500)));
    }

    fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, ev: UserEvent) {
        let UserEvent::Menu(MenuEvent { id, .. }) = ev;
        let Some(items) = &self.items else { return };

        if id == items.enable.id() {
            let now_enabled = items.enable.is_checked();
            self.pipeline.set_enabled(now_enabled);
            let mut c = self.cfg.write().unwrap();
            c.enabled = now_enabled;
            let _ = c.save();
            info!(enabled = now_enabled, "user toggled enable");
        } else if id == items.open_logs.id() {
            let _ = std::process::Command::new("explorer")
                .arg(crate::config::log_dir())
                .spawn();
        } else if id == items.quit.id() {
            info!("quit requested");
            event_loop.exit();
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
                let _ = tray.set_tooltip(Some(tooltip(&self.pipeline)));
            }
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
