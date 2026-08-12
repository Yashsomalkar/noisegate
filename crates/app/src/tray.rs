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
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::event_loop::{ControlFlow, EventLoop};

use audio_io::devices::DeviceList;

use crate::config::Config;
use crate::parking_lot_compat::RwLock;
use crate::pipeline::Pipeline;

/// Where to send someone who has no virtual audio cable. The vendor's page
/// rather than a direct installer link: they should see the licence terms and
/// download the current signed build, not one we pinned months ago.
const CABLE_DOWNLOAD_URL: &str = "https://vb-audio.com/Cable/";

/// `startup_error`, if any, is reported *after* the tray icon exists — a modal
/// dialog with nothing behind it reads as an error from nowhere.
pub fn run(
    cfg: Arc<RwLock<Config>>,
    pipeline: Option<Pipeline>,
    startup_error: Option<crate::StartupProblem>,
) -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    // Forward tray events to winit so we run on a single event loop.
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = menu_proxy.send_event(UserEvent::Menu(e));
    }));
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Tray(e));
    }));

    let mut app = App {
        cfg,
        pipeline,
        startup_error,
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
    Tray(TrayIconEvent),
}

struct App {
    cfg: Arc<RwLock<Config>>,
    /// `None` only while a device switch is restarting it, or if a restart
    /// failed — the tray stays alive either way so the user can pick again.
    pipeline: Option<Pipeline>,
    /// Shown once, after the icon is up.
    startup_error: Option<crate::StartupProblem>,
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
                // Badge first, so it's already showing behind the dialog.
                self.refresh_icon();
                crate::message_box(&format!("Could not start audio with that device:\n\n{e:#}"));
                return;
            }
        }
        self.refresh_icon();
    }

    /// Keep the icon in step with both bits of state it shows: teal vs orange
    /// for on/off, and the badge for "audio isn't running at all".
    fn refresh_icon(&self) {
        if let Some(tray) = &self.tray {
            let enabled = self.cfg.read().unwrap().enabled;
            let _ = tray.set_icon(Some(build_icon(enabled, self.pipeline.is_none())));
        }
    }

    /// The single path for turning denoising on and off, whichever surface
    /// asked — the menu checkbox or a left click on the icon. Both have to
    /// leave the checkbox, the icon and the config agreeing with each other.
    fn set_enabled(&mut self, enabled: bool) {
        if let Some(p) = &self.pipeline {
            p.set_enabled(enabled);
        }
        {
            let mut c = self.cfg.write().unwrap();
            c.enabled = enabled;
            if let Err(e) = c.save() {
                warn!(error = %e, "saving config failed");
            }
        }
        if let Some(items) = &self.items {
            // A left click didn't touch the menu, so sync it by hand.
            if items.enable.is_checked() != enabled {
                items.enable.set_checked(enabled);
            }
        }
        self.refresh_icon();
        info!(enabled, "denoising toggled");
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

        let enabled = self.cfg.read().unwrap().enabled;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip)
            .with_icon(build_icon(enabled, self.pipeline.is_none()))
            // Left click is the on/off toggle, so it must not also open the
            // menu. Right click still does.
            .with_menu_on_left_click(false)
            .build()
            .expect("build tray icon");

        self.tray = Some(tray);
        self.items = Some(items);

        // Tick periodically so we can refresh the tooltip CPU meter.
        event_loop.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(500)));

        // Now that there's an icon in the tray, it's safe to interrupt with a
        // dialog: the user can see what it belongs to, and the badge is still
        // there once they dismiss it.
        if let Some(problem) = self.startup_error.take() {
            if problem.missing_cable {
                // The only actionable fix is installing one, so offer to take
                // them there rather than leaving them to search for it.
                let text = format!(
                    "{}\n\nA virtual audio cable is what lets other apps hear the cleaned \
                     microphone. Open the VB-Cable download page now?",
                    problem.message
                );
                if crate::message_box_yes_no(&text) {
                    open_url(CABLE_DOWNLOAD_URL);
                }
            } else {
                crate::message_box(&problem.message);
            }
        }
    }

    fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, ev: UserEvent) {
        let id = match ev {
            UserEvent::Menu(MenuEvent { id, .. }) => id,
            // Left click toggles; releases only, so press-and-drag-away is not
            // a toggle. Right click is handled by the menu itself.
            UserEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) => {
                let now = !self.cfg.read().unwrap().enabled;
                self.set_enabled(now);
                return;
            }
            UserEvent::Tray(_) => return,
        };
        let Some(items) = &self.items else { return };

        if id == *items.enable.id() {
            let now_enabled = items.enable.is_checked();
            self.set_enabled(now_enabled);
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

/// Hand a URL to Explorer, which opens it in the default browser. Same
/// absolute-path treatment as the log folder: never resolved via PATH.
fn open_url(url: &str) {
    let _ = std::process::Command::new(explorer_path()).arg(url).spawn();
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

/// Icons are generated procedurally so v1 doesn't need to ship a .ico.
/// 32x32 rather than 16x16: the warning badge needs the room, and Windows
/// scales down more gracefully than up.
const ICON_SIZE: usize = 32;

/// Is a pixel inside a triangle that has its apex at the top and widens
/// linearly to `max_half` at `bottom`?
fn in_triangle(x: usize, y: usize, top: usize, bottom: usize, cx: usize, max_half: f32) -> bool {
    if y < top || y > bottom {
        return false;
    }
    let t = (y - top) as f32 / (bottom - top) as f32;
    (x as f32 - cx as f32).abs() <= t * max_half
}

/// Denoising is on — the same blue-green the app has always used.
const ACTIVE: [u8; 4] = [0x2a, 0xa1, 0x98, 0xff];
/// Denoising is bypassed. Orange rather than a dimmed teal: at tray size a
/// brightness difference is invisible, a hue change isn't.
const BYPASSED: [u8; 4] = [0xd0, 0x6b, 0x18, 0xff];

/// The tray icon: a disc coloured by whether denoising is on, plus a warning
/// badge when audio isn't running at all.
///
/// Both states matter because the icon is the only surface this app has. Without
/// the colour there's no way to tell a bypassed NoiseGate from a working one;
/// without the badge, a *stopped* one looks identical too, and the error dialog
/// is long gone by the time anyone notices their microphone is dead.
fn icon_rgba(enabled: bool, warning: bool) -> Vec<u8> {
    let mut rgba = vec![0u8; ICON_SIZE * ICON_SIZE * 4];
    let mut put = |x: usize, y: usize, c: [u8; 4]| {
        let i = (y * ICON_SIZE + x) * 4;
        rgba[i..i + 4].copy_from_slice(&c);
    };

    let disc = if enabled { ACTIVE } else { BYPASSED };
    let centre = ICON_SIZE as i32 / 2;
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let d2 = (x as i32 - centre).pow(2) + (y as i32 - centre).pow(2);
            if d2 <= 14 * 14 {
                put(x, y, disc);
            }
        }
    }
    if !warning {
        return rgba;
    }

    // Warning badge, bottom-right: dark triangle outline, amber fill, and an
    // exclamation mark punched back out in the dark colour.
    const DARK: [u8; 4] = [0x1c, 0x1c, 0x1c, 0xff];
    const AMBER: [u8; 4] = [0xf5, 0xa6, 0x23, 0xff];
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            if in_triangle(x, y, 14, 31, 23, 9.0) {
                put(x, y, DARK);
            }
            if in_triangle(x, y, 17, 29, 23, 6.0) {
                put(x, y, AMBER);
            }
        }
    }
    for y in 20..=25 {
        put(22, y, DARK);
        put(23, y, DARK);
    }
    for y in 27..=28 {
        put(22, y, DARK);
        put(23, y, DARK);
    }
    rgba
}

fn build_icon(enabled: bool, warning: bool) -> tray_icon::Icon {
    tray_icon::Icon::from_rgba(
        icon_rgba(enabled, warning),
        ICON_SIZE as u32,
        ICON_SIZE as u32,
    )
    .expect("valid icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cable_link_points_at_the_vendor_over_https() {
        // A typo here sends users somewhere arbitrary to download a kernel
        // driver, so pin the expectation.
        assert_eq!(CABLE_DOWNLOAD_URL, "https://vb-audio.com/Cable/");
        assert!(CABLE_DOWNLOAD_URL.starts_with("https://"));
    }

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

    fn count(px: &[u8], colour: [u8; 4]) -> usize {
        px.chunks(4).filter(|p| *p == colour).count()
    }

    #[test]
    fn every_icon_is_the_size_tray_icon_expects() {
        for enabled in [false, true] {
            for warning in [false, true] {
                assert_eq!(
                    icon_rgba(enabled, warning).len(),
                    ICON_SIZE * ICON_SIZE * 4
                );
            }
        }
    }

    #[test]
    fn on_and_off_are_different_colours_not_just_different_brightness() {
        let on = icon_rgba(true, false);
        let off = icon_rgba(false, false);
        assert_ne!(on, off);
        assert!(count(&on, ACTIVE) > 400 && count(&on, BYPASSED) == 0);
        assert!(count(&off, BYPASSED) > 400 && count(&off, ACTIVE) == 0);

        // Hue must actually differ: at tray size a brightness-only change is
        // invisible. Teal is green-dominant, the bypass colour red-dominant.
        assert!(ACTIVE[1] > ACTIVE[0], "active should be green-dominant");
        assert!(BYPASSED[0] > BYPASSED[1], "bypassed should be red-dominant");
    }

    #[test]
    fn the_warning_badge_is_visible_in_both_toggle_states() {
        const AMBER: [u8; 4] = [0xf5, 0xa6, 0x23, 0xff];
        for enabled in [false, true] {
            let ok = icon_rgba(enabled, false);
            let bad = icon_rgba(enabled, true);
            assert_ne!(ok, bad, "error state must look different");
            assert!(
                count(&bad, AMBER) > 20,
                "badge too small to see: {} px",
                count(&bad, AMBER)
            );
            assert_eq!(count(&ok, AMBER), 0);
        }
    }

    #[test]
    fn the_badge_sits_in_the_corner_not_over_the_whole_icon() {
        let bad = icon_rgba(true, true);
        // Top-left must stay the plain disc colour, so the icon is still
        // recognisable and the on/off hue still readable.
        let i = (8 * ICON_SIZE + 12) * 4;
        assert_eq!(&bad[i..i + 4], &ACTIVE);
    }
}
