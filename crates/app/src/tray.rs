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
use crate::pipeline::{Output, Pipeline};

/// Official VB-Cable download page. We send people there rather than
/// bundling the driver: VB-Audio's license requires a separate agreement for
/// redistribution, and a kernel driver should come from its vendor anyway.
const VBCABLE_URL: &str = "https://vb-audio.com/Cable/";

fn open_url(url: &str) {
    let _ = std::process::Command::new("explorer").arg(url).spawn();
}

/// Menu text for the virtual-mic status item, and whether it's clickable
/// (only when there's something to install).
fn cable_item_text(p: Option<&Pipeline>) -> (String, bool) {
    if let Some(m) = p.and_then(|p| p.rate_mismatch()) {
        return (
            format!(
                "⚠ Virtual mic is {}, needs {} — fix…",
                m.mic_format(),
                m.speaker_format()
            ),
            true,
        );
    }
    match p.map(|p| p.output()) {
        Some(Output::Cable(c)) => (format!("Virtual mic: \"{}\"", c.mic_name), false),
        Some(Output::Manual) => ("Output: custom device (--out / config)".into(), false),
        Some(Output::DefaultSpeakers) | None => {
            ("Install a virtual mic (VB-Cable)…".into(), true)
        }
    }
}

/// Explain the sample-rate mismatch and open the Recording tab of the Sound
/// control panel, where the user can change the mic side's format.
fn show_rate_fix(m: &crate::pipeline::RateMismatch) {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDOK, MB_ICONWARNING, MB_OKCANCEL, MB_SETFOREGROUND,
    };
    let text = format!(
        "Your virtual mic \"{mic}\" is set to {mic_fmt}, but NoiseGate's output into \
         it is {spk_fmt}. This cable doesn't convert between them, so other apps hear \
         you pitched down and warped.\n\n\
         To fix it (once):\n\
         1. In the Sound window that opens, go to the Recording tab.\n\
         2. Right-click \"{mic}\" → Properties → Advanced.\n\
         3. Set Default Format to {spk_fmt} and click OK.\n\
         4. In the NoiseGate tray menu, choose \"Refresh devices\".\n\n\
         If that format isn't offered, this cable can't carry NoiseGate's audio \
         correctly — install VB-Cable instead (tray menu → Output).\n\n\
         Open Sound settings now?",
        mic = m.mic_name,
        mic_fmt = m.mic_format(),
        spk_fmt = m.speaker_format(),
    );
    let answer = unsafe {
        MessageBoxW(
            HWND::default(),
            &HSTRING::from(text),
            &HSTRING::from("NoiseGate — virtual mic format"),
            MB_OKCANCEL | MB_ICONWARNING | MB_SETFOREGROUND,
        )
    };
    if answer == IDOK {
        // ",,1" selects the Recording tab.
        let _ = std::process::Command::new("control").arg("mmsys.cpl,,1").spawn();
    }
}

/// Ask once whether to open the VB-Cable download page. Blocks the (window-
/// less) event loop briefly, which is fine at startup.
fn prompt_install_vbcable(cfg: &Arc<RwLock<Config>>) {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONINFORMATION, MB_SETFOREGROUND, MB_YESNO,
    };
    let text = "NoiseGate needs a virtual microphone so Discord, Zoom, Teams and OBS \
                can use the cleaned audio, and none was found on this PC.\n\n\
                The free VB-Cable driver from VB-Audio provides one.\n\n\
                Open the VB-Cable download page now?\n\n\
                To install: unzip it, right-click VBCABLE_Setup_x64.exe → Run as \
                administrator, then restart Windows if asked. Afterwards choose \
                \"Refresh devices\" in the NoiseGate tray menu (or restart NoiseGate) \
                and select \"CABLE Output (VB-Audio Virtual Cable)\" as your mic in \
                other apps.\n\n\
                \"No\" won't ask again; the tray menu keeps an install link.";
    let answer = unsafe {
        MessageBoxW(
            HWND::default(),
            &HSTRING::from(text),
            &HSTRING::from("NoiseGate — no virtual microphone"),
            MB_YESNO | MB_ICONINFORMATION | MB_SETFOREGROUND,
        )
    };
    if answer == IDYES {
        info!("opening VB-Cable download page");
        open_url(VBCABLE_URL);
    } else {
        let mut c = cfg.write().unwrap();
        c.vbcable_prompt_dismissed = true;
        let _ = c.save();
        info!("VB-Cable prompt dismissed");
    }
}

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
        icon_state: std::cell::Cell::new(IconState::On),
        last_auto_restart: Instant::now(),
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
    /// Last automatic restart attempt (device errors, failed restarts).
    last_auto_restart: Instant,
    /// What the icon currently shows; redrawn only when this changes.
    icon_state: std::cell::Cell<IconState>,
}

struct Items {
    enable: CheckMenuItem,
    /// Microphone submenu; rebuilt by "Refresh devices".
    mic_menu: Submenu,
    /// (item, capture device id — empty = system default)
    mics: Vec<(CheckMenuItem, String)>,
    refresh: MenuItem,
    /// Output submenu; rebuilt by "Refresh devices".
    out_menu: Submenu,
    /// (item, render device id — empty = automatic)
    outs: Vec<(CheckMenuItem, String)>,
    /// Virtual-mic status / "install VB-Cable" link.
    cable: MenuItem,
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

/// Output submenu: Automatic, then virtual cables (what other apps will
/// select as their mic), then every other output device.
fn fill_out_menu(menu: &Submenu, selected: &str) -> Vec<(CheckMenuItem, String)> {
    let mut out = vec![(
        CheckMenuItem::new("Automatic (best virtual mic)", true, selected.is_empty(), None),
        String::new(),
    )];
    let render = audio_io::DeviceList::enumerate()
        .map(|l| l.render)
        .unwrap_or_default();
    let (cables, others): (Vec<_>, Vec<_>) =
        render.into_iter().partition(|d| d.virtual_cable().is_some());
    for d in &cables {
        let c = d.virtual_cable().unwrap();
        let label = format!("{} — apps use \"{}\"", c.label, c.mic_name);
        out.push((CheckMenuItem::new(label, true, d.id == selected, None), d.id.clone()));
    }
    let first_other = out.len();
    for d in &others {
        let label = format!("{} (not a mic)", d.friendly_name);
        out.push((CheckMenuItem::new(label, true, d.id == selected, None), d.id.clone()));
    }
    for (i, (item, _)) in out.iter().enumerate() {
        if i == first_other && first_other < out.len() {
            menu.append(&PredefinedMenuItem::separator()).ok();
        }
        menu.append(item).ok();
    }
    out
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
        self.refresh_cable_item();
        self.refresh_tooltip();
    }

    fn refresh_cable_item(&self) {
        if let Some(items) = &self.items {
            let (text, enabled) = cable_item_text(self.pipeline.as_ref());
            items.cable.set_text(text);
            items.cable.set_enabled(enabled);
        }
    }

    fn refresh_tooltip(&self) {
        if let Some(tray) = &self.tray {
            let text = match (&self.pipeline, &self.last_error) {
                (Some(p), _) if p.rate_mismatch().is_some() => {
                    format!("{}\n⚠ Virtual mic format mismatch", tooltip(p))
                }
                (Some(p), _) if matches!(p.output(), Output::DefaultSpeakers) => {
                    format!("{}\nNo virtual mic — see tray menu", tooltip(p))
                }
                (Some(p), _) => tooltip(p),
                (None, Some(e)) => format!("NoiseGate — stopped: {e}"),
                (None, None) => "NoiseGate — stopped".into(),
            };
            // Windows truncates tray tooltips at 127 chars.
            let _ = tray.set_tooltip(Some(text.chars().take(127).collect::<String>()));

            let state = icon_state(self.pipeline.as_ref());
            if state != self.icon_state.get() {
                self.icon_state.set(state);
                let _ = tray.set_icon(Some(build_icon(state)));
            }
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
        let enable = CheckMenuItem::new("Noise removal", true, enabled, None);

        let mic_menu = Submenu::new("Microphone", true);
        let mics = fill_mic_menu(&mic_menu, &cfg.input_device_id);
        let refresh = MenuItem::new("Refresh devices", true, None);
        let out_menu = Submenu::new("Output (virtual mic)", true);
        let outs = fill_out_menu(&out_menu, &cfg.output_device_id);
        let (cable_text, cable_enabled) = cable_item_text(self.pipeline.as_ref());
        let cable = MenuItem::new(cable_text, cable_enabled, None);

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
        menu.append(&out_menu).ok();
        menu.append(&model_menu).ok();
        menu.append(&refresh).ok();
        menu.append(&cable).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&open_logs).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&quit).ok();

        let state = icon_state(self.pipeline.as_ref());
        self.icon_state.set(state);
        let icon = build_icon(state);
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
            out_menu,
            outs,
            cable,
            models,
            strengths,
            open_logs,
            quit,
        });

        // Tick periodically so we can refresh the tooltip CPU meter.
        event_loop.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(500)));

        let no_cable = matches!(
            self.pipeline.as_ref().map(|p| p.output()),
            Some(Output::DefaultSpeakers)
        );
        if no_cable && !cfg.vbcable_prompt_dismissed {
            prompt_install_vbcable(&self.cfg);
        }
        // Broken audio until fixed, so ask on every launch while it persists.
        if let Some(m) = self.pipeline.as_ref().and_then(|p| p.rate_mismatch()) {
            show_rate_fix(m);
        }
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
        } else if let Some((_, dev_id)) = items.outs.iter().find(|(i, _)| i.id() == &id) {
            let dev_id = dev_id.clone();
            check_only(&items.outs, &id);
            let mut c = self.cfg.write().unwrap();
            if c.output_device_id != dev_id {
                c.output_device_id = dev_id;
                let _ = c.save();
                info!(output = %c.output_device_id, "user picked output");
                restart = true;
            }
        } else if id == items.refresh.id() {
            for (item, _) in items.mics.drain(..) {
                items.mic_menu.remove(&item).ok();
            }
            // Remove everything (items and separators) from the output menu.
            for item in items.out_menu.items() {
                use tray_icon::menu::MenuItemKind as K;
                let _ = match &item {
                    K::MenuItem(i) => items.out_menu.remove(i),
                    K::Check(i) => items.out_menu.remove(i),
                    K::Predefined(i) => items.out_menu.remove(i),
                    K::Submenu(i) => items.out_menu.remove(i),
                    K::Icon(i) => items.out_menu.remove(i),
                };
            }
            items.outs.clear();
            let (selected, selected_out, auto_output) = {
                let c = self.cfg.read().unwrap();
                (
                    c.input_device_id.clone(),
                    c.output_device_id.clone(),
                    c.output_device_id.is_empty(),
                )
            };
            items.outs = fill_out_menu(&items.out_menu, &selected_out);
            items.mics = fill_mic_menu(&items.mic_menu, &selected);
            info!(count = items.mics.len() - 1, "microphone list refreshed");
            // A virtual cable may have just been installed: re-resolve the
            // output if we're on the speaker fallback (or stopped).
            let on_fallback = !matches!(
                self.pipeline.as_ref().map(|p| p.output()),
                Some(Output::Cable(_)) | Some(Output::Manual)
            );
            // Also re-check after the user fixed a sample-rate mismatch.
            let mismatch = self.pipeline.as_ref().is_some_and(|p| p.rate_mismatch().is_some());
            if (auto_output && on_fallback) || mismatch {
                restart = true;
            }
        } else if id == items.cable.id() {
            match self.pipeline.as_ref().and_then(|p| p.rate_mismatch()) {
                Some(m) => show_rate_fix(m),
                None => open_url(VBCABLE_URL),
            }
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
        } else {
            // e.g. the Enabled toggle: update icon/tooltip right away.
            self.refresh_tooltip();
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
        // Self-heal: if a stream died (device invalidated, unplugged, taken
        // exclusively) or a restart failed, retry — at most every 3 s.
        let broken = match &self.pipeline {
            Some(p) => !p.is_healthy(),
            None => true,
        };
        if broken && self.last_auto_restart.elapsed() >= Duration::from_secs(3) {
            self.last_auto_restart = Instant::now();
            tracing::warn!("audio stream stopped (device error); restarting");
            self.restart_pipeline();
        }

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
        if p.is_enabled() { "Noise removal ON" } else { "Noise removal OFF (raw mic passes through)" },
        cpu_pct,
        peak_ns as f64 / 1_000_000.0,
    )
}

/// What the tray icon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconState {
    /// Denoising into a working virtual mic.
    On,
    /// Running with noise removal off: the raw mic passes through.
    Bypass,
    /// Stopped, no virtual mic, or a cable format mismatch.
    Problem,
}

fn icon_state(p: Option<&Pipeline>) -> IconState {
    match p {
        None => IconState::Problem,
        Some(p) if p.rate_mismatch().is_some() => IconState::Problem,
        Some(p) if matches!(p.output(), Output::DefaultSpeakers) => IconState::Problem,
        Some(p) if !p.is_enabled() => IconState::Bypass,
        Some(_) => IconState::On,
    }
}

/// Tray icon size; drawn at this resolution, Windows scales for the tray.
const ICON_N: usize = 32;

/// Draw the tray icon procedurally (no .ico, no build.rs), 32x32 RGBA,
/// anti-aliased by 4x4 supersampling. Kept to one bold shape per state so it
/// stays readable at 16 px:
/// - On: teal disc, a wiggly line that turns straight (noise in, clean out).
/// - Off (noise removal disabled): grey disc, the line wiggly all the way.
/// - Problem: amber disc with a large "!".
fn icon_rgba(state: IconState) -> Vec<u8> {
    const N: usize = ICON_N;
    const SS: usize = 4;
    const WHITE: [u8; 3] = [0xff, 0xff, 0xff];
    let bg: [u8; 3] = match state {
        IconState::On => [0x14, 0x8f, 0x82],      // teal
        IconState::Bypass => [0x6b, 0x72, 0x7a],  // grey
        IconState::Problem => [0xe0, 0x8a, 0x10], // amber
    };

    // The line as a polyline: y = 16 + A(x)·sin(...), x from 4.5 to 27.5.
    // On: full wiggle until x≈13.5, fading to flat by x≈18. Off: wiggle throughout.
    let amp = |x: f32| -> f32 {
        const A: f32 = 5.0;
        match state {
            IconState::On => {
                if x <= 13.5 {
                    A
                } else if x >= 18.0 {
                    0.0
                } else {
                    A * (1.0 - (x - 13.5) / 4.5)
                }
            }
            _ => A,
        }
    };
    let line: Vec<(f32, f32)> = (0..=184)
        .map(|i| {
            let x = 4.5 + i as f32 * 0.125;
            let y = 16.0 + amp(x) * ((x - 4.5) / 9.0 * std::f32::consts::TAU).sin();
            (x, y)
        })
        .collect();
    let on_line = |x: f32, y: f32| {
        line.iter().any(|&(lx, ly)| (x - lx).powi(2) + (y - ly).powi(2) <= 1.55f32.powi(2))
    };
    let disc = |x: f32, y: f32| (x - 16.0).powi(2) + (y - 16.0).powi(2) <= 15.5f32.powi(2);
    // Large exclamation mark for problems.
    let bang = |x: f32, y: f32| {
        let bar = {
            // Slight taper: wider at the top.
            let half = 2.4 - (y - 6.0) * 0.06;
            (y >= 6.0 && y <= 19.5) && (x - 16.0).abs() <= half
        };
        let dot = (x - 16.0).powi(2) + (y - 24.5).powi(2) <= 2.5f32.powi(2);
        bar || dot
    };

    let pixel = |x: f32, y: f32| -> Option<[u8; 3]> {
        if !disc(x, y) {
            return None;
        }
        let ink = match state {
            IconState::Problem => bang(x, y),
            _ => on_line(x, y),
        };
        Some(if ink { WHITE } else { bg })
    };

    let mut rgba = vec![0u8; N * N * 4];
    for py in 0..N {
        for px in 0..N {
            let (mut r, mut g, mut b, mut a) = (0f32, 0f32, 0f32, 0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                    if let Some([cr, cg, cb]) = pixel(x, y) {
                        r += cr as f32;
                        g += cg as f32;
                        b += cb as f32;
                        a += 1.0;
                    }
                }
            }
            let i = (py * N + px) * 4;
            if a > 0.0 {
                rgba[i] = (r / a) as u8;
                rgba[i + 1] = (g / a) as u8;
                rgba[i + 2] = (b / a) as u8;
                rgba[i + 3] = (a / (SS * SS) as f32 * 255.0) as u8;
            }
        }
    }
    rgba
}

fn build_icon(state: IconState) -> tray_icon::Icon {
    tray_icon::Icon::from_rgba(icon_rgba(state), ICON_N as u32, ICON_N as u32)
        .expect("valid icon")
}

/// `--dump-icons <dir>`: write each state's icon as raw 32x32 RGBA
/// (`on.rgba`, `bypass.rgba`, `problem.rgba`) for previewing, e.g.
/// `ffmpeg -f rawvideo -pix_fmt rgba -s 32x32 -i on.rgba on.png`.
pub fn dump_icons(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, state) in [
        ("on", IconState::On),
        ("bypass", IconState::Bypass),
        ("problem", IconState::Problem),
    ] {
        std::fs::write(dir.join(format!("{name}.rgba")), icon_rgba(state))?;
    }
    Ok(())
}
