//! System tray UI: status header, noise-removal toggle (also a left click on
//! the icon), device and model pickers, start with Windows, quit. The icon
//! morphs between its states (see `Look`).
//!
//! `tray-icon` needs a window-message pump on the main thread, so we drive
//! it with a `winit` event loop (no actual window — just the tray icon).

#![cfg(windows)]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{error, info};
use tray_icon::menu::{
    CheckMenuItem, Icon as MenuIcon, IconMenuItem, Menu, MenuEvent, MenuId, MenuItem,
    PredefinedMenuItem, Submenu,
};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
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
        Some(Output::Cable(c)) => (format!("In other apps, pick \"{}\"", c.mic_name), false),
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

/// How long the icon takes to morph between states, and its frame interval.
const ANIM_DURATION: Duration = Duration::from_millis(550);
const ANIM_FRAME: Duration = Duration::from_millis(33);
/// Tooltip / health refresh interval when nothing is animating.
const IDLE_TICK: Duration = Duration::from_millis(500);

pub fn run(cfg: Arc<RwLock<Config>>, pipeline: Pipeline) -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;

    // Forward menu and tray-icon events to winit so we run on a single
    // event loop.
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));
    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Tray(e));
    }));

    let mut app = App {
        cfg,
        pipeline: Some(pipeline),
        last_error: None,
        tray: None,
        items: None,
        last_tooltip_update: Instant::now(),
        icon_state: IconState::Bypass,
        shown: look(IconState::Bypass),
        anim: None,
        last_auto_restart: Instant::now(),
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
    /// None only while a restart failed (e.g. the picked mic vanished).
    pipeline: Option<Pipeline>,
    last_error: Option<String>,
    tray: Option<TrayIcon>,
    items: Option<Items>,
    last_tooltip_update: Instant,
    /// Last automatic restart attempt (device errors, failed restarts).
    last_auto_restart: Instant,
    /// The state the icon shows (or is animating towards).
    icon_state: IconState,
    /// What the icon looks like right now (mid-animation: in between).
    shown: Look,
    anim: Option<IconAnim>,
}

/// A running icon morph from `from` to the look of `to`.
struct IconAnim {
    from: Look,
    to: IconState,
    start: Instant,
    last_frame: Instant,
}

struct Items {
    /// Status line at the top, with the state icon.
    header: IconMenuItem,
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
    /// What to select in other apps / "install VB-Cable" link / format fix.
    cable: MenuItem,
    model_menu: Submenu,
    /// (item, backend key)
    models: Vec<(CheckMenuItem, &'static str)>,
    strength_menu: Submenu,
    /// (item, attenuation limit dB)
    strengths: Vec<(CheckMenuItem, f32)>,
    autostart: CheckMenuItem,
    open_logs: MenuItem,
    quit: MenuItem,
}

/// Check exactly the item whose id is `id` (radio-group behaviour).
fn check_only<T>(group: &[(CheckMenuItem, T)], id: &MenuId) {
    for (item, _) in group {
        item.set_checked(item.id() == id);
    }
}

/// Text of the checked item in a radio group.
fn checked_text<T>(group: &[(CheckMenuItem, T)]) -> Option<String> {
    group.iter().find(|(i, _)| i.is_checked()).map(|(i, _)| i.text())
}

/// Shorten `s` to at most `max` characters, ending in "…" if cut.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

/// First word of a model label / runtime name ("DeepFilterNet3 (high …)").
fn first_word(s: &str) -> &str {
    s.split(' ').next().unwrap_or(s)
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
        let label = format!("{}  —  apps use \"{}\"", c.label, c.mic_name);
        out.push((CheckMenuItem::new(label, true, d.id == selected, None), d.id.clone()));
    }
    let first_other = out.len();
    for d in &others {
        let label = format!("{} (speaker, not a mic)", d.friendly_name);
        out.push((CheckMenuItem::new(label, true, d.id == selected, None), d.id.clone()));
    }
    for (i, (item, _)) in out.iter().enumerate() {
        if i == 1 || (i == first_other && first_other < out.len()) {
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
    for (i, (item, _)) in out.iter().enumerate() {
        if i == 1 {
            menu.append(&PredefinedMenuItem::separator()).ok();
        }
        menu.append(item).ok();
    }
    out
}

/// Remove every entry (items and separators) from a submenu.
fn clear_submenu(menu: &Submenu) {
    use tray_icon::menu::MenuItemKind as K;
    for item in menu.items() {
        let _ = match &item {
            K::MenuItem(i) => menu.remove(i),
            K::Check(i) => menu.remove(i),
            K::Predefined(i) => menu.remove(i),
            K::Submenu(i) => menu.remove(i),
            K::Icon(i) => menu.remove(i),
        };
    }
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
        self.refresh_ui();
    }

    fn set_enabled(&mut self, on: bool) {
        if let Some(items) = &self.items {
            items.enable.set_checked(on);
        }
        if let Some(p) = &self.pipeline {
            p.set_enabled(on);
        }
        let mut c = self.cfg.write().unwrap();
        c.enabled = on;
        let _ = c.save();
        info!(enabled = on, "user toggled noise removal");
    }

    fn is_enabled(&self) -> bool {
        match &self.pipeline {
            Some(p) => p.is_enabled(),
            None => self.cfg.read().unwrap().enabled,
        }
    }

    /// Short name of the active (or configured) model.
    fn model_name(&self) -> String {
        if let Some(p) = &self.pipeline {
            return first_word(p.denoiser_name()).to_string();
        }
        let key = self.cfg.read().unwrap().denoiser.clone();
        dsp::BACKENDS
            .iter()
            .find(|b| b.key == key)
            .or(dsp::BACKENDS.first())
            .map(|b| first_word(b.label).to_string())
            .unwrap_or_default()
    }

    fn uses_strength(&self) -> bool {
        self.model_name().starts_with("DeepFilterNet")
    }

    fn strength_name(&self) -> &'static str {
        let db = self.cfg.read().unwrap().attenuation_db;
        STRENGTHS
            .iter()
            .find(|(_, d)| (d - db).abs() < 0.5)
            .map(|(l, _)| first_word(l))
            .unwrap_or("Custom")
    }

    /// One-line status for the menu header and tooltip.
    fn status_text(&self) -> String {
        match &self.pipeline {
            None => match &self.last_error {
                Some(_) => "Audio stopped — retrying…".into(),
                None => "Stopped".into(),
            },
            Some(p) if p.rate_mismatch().is_some() => "Virtual mic format needs fixing".into(),
            Some(p) if matches!(p.output(), Output::DefaultSpeakers) => {
                "No virtual mic installed".into()
            }
            Some(p) if !p.is_enabled() => "Off — mic passes through unfiltered".into(),
            Some(_) => "Removing noise".into(),
        }
    }

    /// Refresh everything that reflects state: icon, tooltip, menu labels.
    fn refresh_ui(&mut self) {
        self.refresh_icon();
        self.refresh_tooltip();
        self.refresh_labels();
    }

    fn refresh_labels(&self) {
        let Some(items) = &self.items else { return };
        let header = format!("NoiseGate  ·  {}", self.status_text());
        if items.header.text() != header {
            items.header.set_text(header);
            items.header.set_icon(Some(menu_icon(self.icon_state)));
        }

        let (text, enabled) = cable_item_text(self.pipeline.as_ref());
        items.cable.set_text(text);
        items.cable.set_enabled(enabled);

        let mic = checked_text(&items.mics).unwrap_or_else(|| "System default".into());
        items.mic_menu.set_text(format!("Microphone:  {}", ellipsize(&mic, 34)));
        let out = match self.pipeline.as_ref().map(|p| p.output()) {
            Some(Output::Cable(c)) => c.label.to_string(),
            Some(Output::Manual) => checked_text(&items.outs)
                .map(|t| ellipsize(&t, 34))
                .unwrap_or_else(|| "custom device".into()),
            Some(Output::DefaultSpeakers) => "none found".into(),
            None => "—".into(),
        };
        items.out_menu.set_text(format!("Virtual mic:  {out}"));
        items.model_menu.set_text(format!("Model:  {}", self.model_name()));
        let uses_strength = self.uses_strength();
        items.strength_menu.set_enabled(uses_strength);
        items.strength_menu.set_text(if uses_strength {
            format!("Strength:  {}", self.strength_name())
        } else {
            "Strength:  (DeepFilterNet3 only)".into()
        });
    }

    fn refresh_tooltip(&self) {
        let Some(tray) = &self.tray else { return };
        let mut text = format!("NoiseGate — {}", self.status_text());
        if let Some(p) = &self.pipeline {
            let model = if self.uses_strength() {
                format!("{} · {}", self.model_name(), self.strength_name())
            } else {
                self.model_name()
            };
            text.push_str(&format!("\n{model} · CPU {:.1}%", cpu_percent(p)));
            text.push_str(if p.is_enabled() {
                "\nClick to turn off"
            } else {
                "\nClick to turn on"
            });
        } else if let Some(e) = &self.last_error {
            text.push('\n');
            text.push_str(e);
        }
        // Windows truncates tray tooltips at 127 chars.
        let _ = tray.set_tooltip(Some(text.chars().take(127).collect::<String>()));
    }

    /// Start a morph if the state changed. The frames are drawn by
    /// `tick_anim` from the event loop.
    fn refresh_icon(&mut self) {
        let state = icon_state(self.pipeline.as_ref());
        if state != self.icon_state {
            self.icon_state = state;
            let now = Instant::now();
            self.anim = Some(IconAnim {
                from: self.shown,
                to: state,
                start: now,
                last_frame: now - ANIM_FRAME,
            });
        }
    }

    /// Draw the next animation frame if one is due.
    fn tick_anim(&mut self) {
        let Some(anim) = &mut self.anim else { return };
        if anim.last_frame.elapsed() < ANIM_FRAME * 3 / 4 {
            return;
        }
        anim.last_frame = Instant::now();
        let t = (anim.start.elapsed().as_secs_f32() / ANIM_DURATION.as_secs_f32()).min(1.0);
        let eased = t * t * (3.0 - 2.0 * t); // smoothstep
        self.shown = anim.from.lerp(&look(anim.to), eased);
        if t >= 1.0 {
            self.anim = None;
        }
        if let Some(tray) = &self.tray {
            let _ = tray.set_icon(Some(build_icon(&self.shown)));
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {
        if self.tray.is_some() {
            return;
        }
        let cfg = self.cfg.read().unwrap().clone();
        let enabled = self.is_enabled();
        crate::theme::sync_menu_theme();

        let menu = Menu::new();
        let header = IconMenuItem::new("NoiseGate", true, None, None);
        let enable = CheckMenuItem::new("Noise removal", true, enabled, None);

        let mic_menu = Submenu::new("Microphone", true);
        let mics = fill_mic_menu(&mic_menu, &cfg.input_device_id);
        let out_menu = Submenu::new("Virtual mic", true);
        let outs = fill_out_menu(&out_menu, &cfg.output_device_id);
        let refresh = MenuItem::new("Refresh devices", true, None);
        let (cable_text, cable_enabled) = cable_item_text(self.pipeline.as_ref());
        let cable = MenuItem::new(cable_text, cable_enabled, None);

        let model_menu = Submenu::new("Model", true);
        let current = self.model_name();
        let models: Vec<_> = dsp::BACKENDS
            .iter()
            .map(|b| {
                let checked = first_word(b.label) == current;
                (CheckMenuItem::new(b.label, true, checked, None), b.key)
            })
            .collect();
        for (item, _) in &models {
            model_menu.append(item).ok();
        }
        let strength_menu = Submenu::new("Strength", true);
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

        let autostart = CheckMenuItem::new(
            "Start with Windows",
            true,
            crate::autostart::is_enabled(),
            None,
        );
        let open_logs = MenuItem::new("Open log folder", true, None);
        let quit = MenuItem::new("Quit NoiseGate", true, None);

        let sep = PredefinedMenuItem::separator;
        menu.append(&header).ok();
        menu.append(&sep()).ok();
        menu.append(&enable).ok();
        menu.append(&sep()).ok();
        menu.append(&mic_menu).ok();
        menu.append(&out_menu).ok();
        menu.append(&cable).ok();
        menu.append(&refresh).ok();
        menu.append(&sep()).ok();
        menu.append(&model_menu).ok();
        menu.append(&strength_menu).ok();
        menu.append(&sep()).ok();
        menu.append(&autostart).ok();
        menu.append(&open_logs).ok();
        menu.append(&sep()).ok();
        menu.append(&quit).ok();

        // Start from the "off" look so the icon visibly settles into its
        // state at launch.
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("NoiseGate — starting")
            .with_icon(build_icon(&self.shown))
            .build()
            .expect("build tray icon");

        self.tray = Some(tray);
        self.items = Some(Items {
            header,
            enable,
            mic_menu,
            mics,
            refresh,
            out_menu,
            outs,
            cable,
            model_menu,
            models,
            strength_menu,
            strengths,
            autostart,
            open_logs,
            quit,
        });
        self.refresh_ui();
        // The header icon is only set when its text changes; make sure the
        // first one matches.
        if let Some(items) = &self.items {
            items.header.set_icon(Some(menu_icon(self.icon_state)));
        }

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
        let id = match ev {
            UserEvent::Menu(MenuEvent { id }) => id,
            // Left click on the icon toggles noise removal; right click opens
            // the menu (handled by tray-icon).
            UserEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) => {
                self.set_enabled(!self.is_enabled());
                self.refresh_ui();
                return;
            }
            UserEvent::Tray(_) => return,
        };
        let Some(items) = &self.items else { return };

        // Device / model changes need a pipeline restart, done after the
        // config write lock is released.
        let mut restart = false;

        if id == items.enable.id() {
            let on = items.enable.is_checked();
            self.set_enabled(on);
        } else if id == items.header.id() {
            // Status line; nothing to do.
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
            let (selected, selected_out, auto_output) = {
                let c = self.cfg.read().unwrap();
                (
                    c.input_device_id.clone(),
                    c.output_device_id.clone(),
                    c.output_device_id.is_empty(),
                )
            };
            let items = self.items.as_mut().unwrap();
            clear_submenu(&items.mic_menu);
            clear_submenu(&items.out_menu);
            items.outs = fill_out_menu(&items.out_menu, &selected_out);
            items.mics = fill_mic_menu(&items.mic_menu, &selected);
            info!(count = items.mics.len() - 1, "device lists refreshed");
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
        } else if id == items.autostart.id() {
            let on = items.autostart.is_checked();
            match crate::autostart::set(on) {
                Ok(()) => {
                    info!(enabled = on, "start with Windows");
                    let mut c = self.cfg.write().unwrap();
                    c.auto_start = on;
                    let _ = c.save();
                }
                Err(e) => {
                    error!(error = %format!("{e:#}"), "changing start with Windows failed");
                    items.autostart.set_checked(crate::autostart::is_enabled());
                }
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
            self.refresh_ui();
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

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
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
            self.refresh_ui();
            crate::theme::sync_menu_theme();
        }

        self.tick_anim();

        // Re-arm the wake-up every time: `wait_duration` is a deadline
        // (now + d), not an interval, so setting it once would leave a past
        // deadline and spin the loop at 100% of a core.
        let next = if self.anim.is_some() { ANIM_FRAME } else { IDLE_TICK };
        event_loop.set_control_flow(ControlFlow::wait_duration(next));
    }
}

fn cpu_percent(p: &Pipeline) -> f64 {
    let s = p.stats();
    let frames = s.frames.load(Ordering::Relaxed);
    let total_ns = s.dsp_ns.load(Ordering::Relaxed);
    // Each frame is 10 ms of audio: CPU% = DSP time / audio time.
    if frames == 0 {
        0.0
    } else {
        (total_ns as f64 / frames as f64) / 1_000_000.0 / 10.0 * 100.0
    }
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

/// The drawable parameters of the icon. Every state is a point in this
/// space, so any two can be blended for the transition animation.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Look {
    /// Disc colour (linear 0–255 per channel).
    bg: [f32; 3],
    /// 0 = wiggly all the way; 1 = straightened from the middle on. In
    /// between, the straight part sweeps in from the right.
    calm: f32,
    /// 0 = the line; 1 = the "!" (the line collapses, then the "!" pops in).
    alert: f32,
}

impl Look {
    fn lerp(&self, to: &Look, t: f32) -> Look {
        let m = |a: f32, b: f32| a + (b - a) * t;
        Look {
            bg: [m(self.bg[0], to.bg[0]), m(self.bg[1], to.bg[1]), m(self.bg[2], to.bg[2])],
            calm: m(self.calm, to.calm),
            alert: m(self.alert, to.alert),
        }
    }
}

fn look(state: IconState) -> Look {
    match state {
        IconState::On => Look { bg: [20.0, 143.0, 130.0], calm: 1.0, alert: 0.0 }, // teal
        IconState::Bypass => Look { bg: [107.0, 114.0, 122.0], calm: 0.0, alert: 0.0 }, // grey
        IconState::Problem => Look { bg: [224.0, 138.0, 16.0], calm: 0.0, alert: 1.0 }, // amber
    }
}

/// Tray icon size; drawn at this resolution, Windows scales for the tray.
const ICON_N: usize = 32;
/// Menu item icons are shown at 16 px; drawn at that size to stay crisp.
const MENU_ICON_N: usize = 16;

/// Draw the icon procedurally (no .ico, no build.rs) as n×n RGBA,
/// anti-aliased by 4x4 supersampling. Geometry is defined on a 32-unit grid
/// and scaled to `n`. One bold shape per state so it reads at 16 px:
/// - On: teal disc, a wiggly line that turns straight (noise in, clean out).
/// - Off (noise removal disabled): grey disc, the line wiggly all the way.
/// - Problem: amber disc with a large "!".
fn icon_rgba(lk: &Look, n: usize) -> Vec<u8> {
    const SS: usize = 4;
    const A: f32 = 5.0;
    const X0: f32 = 4.5;
    const STEP: f32 = 0.125;
    const STEPS: usize = 184; // X0 + 184·0.125 = 27.5
    const HALF_W: f32 = 1.55;

    // Line: y = 16 + amp(x)·sin(...). The straight part starts at `edge`,
    // ramping down over 4.5 units; calm=1 puts it at 13.5 (the "On" look),
    // calm=0 past the end of the line (all wiggly).
    let edge = 27.5 - 14.0 * lk.calm;
    // Problem transition in two halves: the line flattens and shrinks to
    // the centre (alert 0→0.5), then the "!" pops out of it (0.5→1).
    let collapse = (lk.alert * 2.0).clamp(0.0, 1.0);
    let pop = (lk.alert * 2.0 - 1.0).clamp(0.0, 1.0);
    let half_len = 11.5 * (1.0 - collapse);
    let line_amp = A * (1.0 - collapse);
    let line: Vec<(f32, f32)> = (0..=STEPS)
        .map(|i| {
            let x = X0 + i as f32 * STEP;
            let flat = ((x - edge) / 4.5).clamp(0.0, 1.0);
            let y = 16.0 + line_amp * (1.0 - flat) * ((x - X0) / 9.0 * std::f32::consts::TAU).sin();
            (x, y)
        })
        .collect();
    let on_line = |x: f32, y: f32| {
        if collapse >= 1.0 {
            return false;
        }
        // Only points within the stroke width in x can be close enough.
        let lo = (((x - HALF_W - X0) / STEP).floor().max(0.0)) as usize;
        let hi = (((x + HALF_W - X0) / STEP).ceil().max(0.0) as usize).min(STEPS);
        lo <= hi
            && line[lo..=hi].iter().any(|&(lx, ly)| {
                (lx - 16.0).abs() <= half_len
                    && (x - lx).powi(2) + (y - ly).powi(2) <= HALF_W * HALF_W
            })
    };
    let disc = |x: f32, y: f32| (x - 16.0).powi(2) + (y - 16.0).powi(2) <= 15.5f32.powi(2);
    // Large exclamation mark, scaled up from the centre as it pops in.
    let s = pop * pop * (3.0 - 2.0 * pop);
    let bang = |x: f32, y: f32| {
        if s < 0.02 {
            return false;
        }
        let (x, y) = (16.0 + (x - 16.0) / s, 16.0 + (y - 16.0) / s);
        let bar = {
            // Slight taper: wider at the top.
            let half = 2.4 - (y - 6.0) * 0.06;
            (6.0..=19.5).contains(&y) && (x - 16.0).abs() <= half
        };
        let dot = (x - 16.0).powi(2) + (y - 24.5).powi(2) <= 2.5f32.powi(2);
        bar || dot
    };

    let pixel = |x: f32, y: f32| -> Option<[f32; 3]> {
        if !disc(x, y) {
            return None;
        }
        let ink = on_line(x, y) || bang(x, y);
        Some(if ink { [255.0; 3] } else { lk.bg })
    };

    let scale = 32.0 / n as f32;
    let mut rgba = vec![0u8; n * n * 4];
    for py in 0..n {
        for px in 0..n {
            let (mut r, mut g, mut b, mut a) = (0f32, 0f32, 0f32, 0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                    if let Some([cr, cg, cb]) = pixel(x, y) {
                        r += cr;
                        g += cg;
                        b += cb;
                        a += 1.0;
                    }
                }
            }
            let i = (py * n + px) * 4;
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

fn build_icon(lk: &Look) -> tray_icon::Icon {
    tray_icon::Icon::from_rgba(icon_rgba(lk, ICON_N), ICON_N as u32, ICON_N as u32)
        .expect("valid icon")
}

fn menu_icon(state: IconState) -> MenuIcon {
    MenuIcon::from_rgba(icon_rgba(&look(state), MENU_ICON_N), MENU_ICON_N as u32, MENU_ICON_N as u32)
        .expect("valid icon")
}

/// `--dump-icons <dir>`: write each state's icon as raw 32x32 RGBA
/// (`on.rgba`, `bypass.rgba`, `problem.rgba`) plus the transition frames
/// (`anim-<from>-<to>-<i>.rgba`, 8 frames each) for previewing, e.g.
/// `ffmpeg -f rawvideo -pix_fmt rgba -s 32x32 -i on.rgba on.png`.
pub fn dump_icons(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let states = [
        ("on", IconState::On),
        ("bypass", IconState::Bypass),
        ("problem", IconState::Problem),
    ];
    for (name, state) in states {
        std::fs::write(dir.join(format!("{name}.rgba")), icon_rgba(&look(state), ICON_N))?;
    }
    for (from_name, from) in states {
        for (to_name, to) in states {
            if from == to {
                continue;
            }
            for i in 0..8 {
                let t = i as f32 / 7.0;
                let eased = t * t * (3.0 - 2.0 * t);
                let lk = look(from).lerp(&look(to), eased);
                std::fs::write(
                    dir.join(format!("anim-{from_name}-{to_name}-{i}.rgba")),
                    icon_rgba(&lk, ICON_N),
                )?;
            }
        }
    }
    Ok(())
}
