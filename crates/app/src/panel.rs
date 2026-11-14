//! Settings panel: a small flyout above the tray icon (left click), like the
//! Windows volume flyout. Unlike a native popup menu it stays open while you
//! change things, and closes when it loses focus or on Escape.
//!
//! Drawn with egui on an OpenGL (glutin) surface in an undecorated,
//! always-on-top winit window that lives on the tray's event loop. The App
//! hands it a `PanelModel` snapshot each frame and applies the
//! `PanelAction`s it returns, so the panel holds no settings of its own.

#![cfg(windows)]

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use egui::{Color32, CornerRadius, FontFamily, FontId, RichText, Sense, Stroke, StrokeKind, Vec2};
use glutin::context::NotCurrentGlContext as _;
use glutin::display::{GetGlDisplay as _, GlDisplay as _};
use glutin::surface::GlSurface as _;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use winit::window::{Window, WindowAttributes, WindowId, WindowLevel};

use crate::tray::IconState;

/// Panel width in logical pixels; the height follows the content.
const WIDTH: f32 = 360.0;
/// Gap between the panel and the taskbar / screen edge.
const GAP: f64 = 12.0;
const ACCENT: Color32 = Color32::from_rgb(0x14, 0x8f, 0x82); // tray teal
const WARN: Color32 = Color32::from_rgb(0xe0, 0x8a, 0x10); // tray amber

/// Everything the panel shows; built by the App for every frame.
pub struct PanelModel {
    pub icon: IconState,
    pub status: String,
    pub enabled: bool,
    /// (capture device id — empty = system default, label)
    pub mics: Vec<(String, String)>,
    pub mic: String,
    /// (render device id — empty = automatic, label)
    pub outs: Vec<(String, String)>,
    pub out: String,
    /// What to select in other apps, or what's wrong with the virtual mic.
    pub hint: String,
    /// Label of a button next to the hint ("Fix…", "Get VB-Cable…").
    pub hint_button: Option<&'static str>,
    pub warn: bool,
    /// (backend key, short label)
    pub models: Vec<(&'static str, String)>,
    pub model: String,
    /// (attenuation dB, label)
    pub strengths: Vec<(f32, &'static str)>,
    pub strength: f32,
    pub uses_strength: bool,
    pub autostart: bool,
    pub cpu_percent: Option<f64>,
}

/// What the user did in the panel.
#[derive(Debug, Clone)]
pub enum PanelAction {
    SetEnabled(bool),
    PickMic(String),
    PickOutput(String),
    PickModel(&'static str),
    PickStrength(f32),
    SetAutostart(bool),
    HintButton,
    Refresh,
    OpenLogs,
    Quit,
}

pub struct Panel {
    window: Window,
    gl_context: glutin::context::PossiblyCurrentContext,
    gl_surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
    egui: egui_glow::EguiGlow,
    visible: bool,
    /// Set once the first frame is painted, so we never show garbage.
    painted: bool,
    hidden_at: Instant,
    /// When egui asked to be repainted next (animations, hover fades).
    pub repaint_at: Option<Instant>,
    icon: Option<(IconState, egui::TextureHandle)>,
    dark: bool,
}

/// Sent from egui's repaint callback (any thread) to the event loop.
pub struct RepaintRequest(pub Duration);

impl Panel {
    pub fn new<E: From<RepaintRequest> + Send + 'static>(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<E>,
    ) -> Result<Self> {
        use winit::platform::windows::WindowAttributesExtWindows as _;
        let attrs = WindowAttributes::default()
            .with_title("NoiseGate")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_skip_taskbar(true)
            // Drag and drop needs OleInitialize (STA), but this thread already
            // joined the multithreaded COM apartment for device enumeration.
            .with_drag_and_drop(false)
            .with_undecorated_shadow(true)
            .with_inner_size(LogicalSize::new(WIDTH, 420.0));

        let template = glutin::config::ConfigTemplateBuilder::new()
            .prefer_hardware_accelerated(None)
            .with_depth_size(0)
            .with_stencil_size(0)
            .with_transparency(false);
        let (window, gl_config) = glutin_winit::DisplayBuilder::new()
            .with_preference(glutin_winit::ApiPreference::FallbackEgl)
            .with_window_attributes(Some(attrs.clone()))
            .build(event_loop, template, |mut configs| {
                configs.next().expect("no OpenGL config")
            })
            .map_err(|e| anyhow!("OpenGL setup failed: {e}"))?;
        let gl_display = gl_config.display();
        let window = match window {
            Some(w) => w,
            None => glutin_winit::finalize_window(event_loop, attrs, &gl_config)
                .context("creating the panel window")?,
        };
        let raw = window.window_handle().context("panel window handle")?.as_raw();
        let ctx_attrs = glutin::context::ContextAttributesBuilder::new().build(Some(raw));
        let gles_attrs = glutin::context::ContextAttributesBuilder::new()
            .with_context_api(glutin::context::ContextApi::Gles(None))
            .build(Some(raw));
        let not_current = unsafe {
            gl_display
                .create_context(&gl_config, &ctx_attrs)
                .or_else(|_| gl_display.create_context(&gl_config, &gles_attrs))
                .context("creating an OpenGL context")?
        };
        let (w, h): (u32, u32) = window.inner_size().into();
        let surface_attrs =
            glutin::surface::SurfaceAttributesBuilder::<glutin::surface::WindowSurface>::new()
                .build(
                    raw,
                    NonZeroU32::new(w).unwrap_or(NonZeroU32::MIN),
                    NonZeroU32::new(h).unwrap_or(NonZeroU32::MIN),
                );
        let gl_surface = unsafe {
            gl_display
                .create_window_surface(&gl_config, &surface_attrs)
                .context("creating the OpenGL surface")?
        };
        let gl_context = not_current
            .make_current(&gl_surface)
            .context("activating the OpenGL context")?;
        let _ = gl_surface.set_swap_interval(
            &gl_context,
            glutin::surface::SwapInterval::Wait(NonZeroU32::MIN),
        );
        let gl = Arc::new(unsafe {
            glow::Context::from_loader_function(|s| {
                let s = std::ffi::CString::new(s).unwrap_or_default();
                gl_display.get_proc_address(&s)
            })
        });

        let egui = egui_glow::EguiGlow::new(event_loop, gl, None, None, true);
        let proxy = std::sync::Mutex::new(proxy);
        egui.egui_ctx.set_request_repaint_callback(move |info| {
            if let Ok(p) = proxy.lock() {
                let _ = p.send_event(E::from(RepaintRequest(info.delay)));
            }
        });
        install_fonts(&egui.egui_ctx);
        round_corners(&window);

        let mut panel = Self {
            window,
            gl_context,
            gl_surface,
            egui,
            visible: false,
            painted: false,
            hidden_at: Instant::now() - Duration::from_secs(10),
            repaint_at: None,
            icon: None,
            dark: false,
        };
        panel.apply_theme(crate::theme::apps_use_dark());
        Ok(panel)
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// True right after the panel closed by losing focus. A click on the tray
    /// icon first takes focus away (closing the panel) and then arrives as a
    /// click; that click should leave it closed, not reopen it.
    pub fn just_hidden(&self) -> bool {
        self.hidden_at.elapsed() < Duration::from_millis(350)
    }

    /// Open above (or next to) the tray icon at `anchor` (physical pixels).
    pub fn show(&mut self, anchor_pos: PhysicalPosition<f64>, anchor_size: PhysicalSize<u32>) {
        let dark = crate::theme::apps_use_dark();
        if dark != self.dark {
            self.apply_theme(dark);
        }
        self.visible = true;
        self.painted = false;
        self.place(anchor_pos, anchor_size);
        // Hidden windows never get WM_PAINT, so show it cloaked (on screen
        // as far as Windows is concerned, but not drawn) until the first
        // frame is ready; `redraw` uncloaks it.
        cloak(&self.window, true);
        self.window.set_visible(true);
        self.window.request_redraw();
    }

    pub fn hide(&mut self) {
        if self.visible {
            self.visible = false;
            self.window.set_visible(false);
            self.hidden_at = Instant::now();
        }
    }

    pub fn request_redraw(&self) {
        if self.visible {
            self.window.request_redraw();
        }
    }

    /// Handle a window event; returns the user's actions (only from redraws).
    pub fn on_event(&mut self, event: &WindowEvent, model: &PanelModel) -> Vec<PanelAction> {
        match event {
            WindowEvent::RedrawRequested => return self.redraw(model),
            WindowEvent::Focused(false) | WindowEvent::CloseRequested => {
                self.hide();
                return Vec::new();
            }
            WindowEvent::KeyboardInput { event: key, .. }
                if key.state.is_pressed()
                    && key.logical_key
                        == winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape) =>
            {
                self.hide();
                return Vec::new();
            }
            WindowEvent::Resized(size) => {
                if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
                    self.gl_surface.resize(&self.gl_context, w, h);
                }
            }
            _ => {}
        }
        if self.egui.on_window_event(&self.window, event).repaint {
            self.window.request_redraw();
        }
        Vec::new()
    }

    fn redraw(&mut self, model: &PanelModel) -> Vec<PanelAction> {
        if !self.visible {
            return Vec::new();
        }
        self.repaint_at = None;
        if self.icon.as_ref().map(|(s, _)| *s) != Some(model.icon) {
            let n = 64;
            let rgba = crate::tray::state_icon_rgba(model.icon, n);
            let image = egui::ColorImage::from_rgba_unmultiplied([n, n], &rgba);
            let tex = self.egui.egui_ctx.load_texture("state", image, egui::TextureOptions::LINEAR);
            self.icon = Some((model.icon, tex));
        }
        let icon = self.icon.as_ref().map(|(_, t)| t.id());

        let mut actions = Vec::new();
        let mut content_h = 0.0;
        let dark = self.dark;
        self.egui.run(&self.window, |ui| {
            let frame = egui::Frame::new()
                .fill(ui.visuals().panel_fill)
                .inner_margin(egui::Margin::same(16));
            egui::CentralPanel::default().frame(frame).show(ui, |ui| {
                let top = ui.next_widget_position().y;
                draw(ui, model, icon, dark, &mut actions);
                content_h = ui.next_widget_position().y - top + 32.0;
            });
        });

        // Fit the window to the content (takes effect next frame).
        let want = LogicalSize::new(WIDTH, content_h.ceil());
        let have: LogicalSize<f32> = self.window.inner_size().to_logical(self.window.scale_factor());
        if (have.height - want.height).abs() > 1.0 {
            let old_bottom = self.window.outer_position().ok().map(|p| p.y + self.window.outer_size().height as i32);
            let _ = self.window.request_inner_size(want);
            // Keep the bottom edge where it was (the panel grows upwards).
            if let Some(bottom) = old_bottom {
                let h = (want.height as f64 * self.window.scale_factor()) as i32;
                if let Ok(p) = self.window.outer_position() {
                    self.window.set_outer_position(PhysicalPosition::new(p.x, bottom - h));
                }
            }
            self.window.request_redraw();
            if !self.painted {
                // Don't show a frame laid out for the wrong height.
                return actions;
            }
        }

        unsafe {
            use glow::HasContext as _;
            let c = self.egui.egui_ctx.global_style().visuals.panel_fill;
            self.egui.painter.gl().clear_color(
                c.r() as f32 / 255.0,
                c.g() as f32 / 255.0,
                c.b() as f32 / 255.0,
                1.0,
            );
            self.egui.painter.gl().clear(glow::COLOR_BUFFER_BIT);
        }
        self.egui.paint(&self.window);
        let _ = self.gl_surface.swap_buffers(&self.gl_context);
        if !self.painted {
            self.painted = true;
            cloak(&self.window, false);
            self.window.focus_window();
            force_foreground(&self.window);
        }
        actions
    }

    fn place(&mut self, anchor_pos: PhysicalPosition<f64>, anchor_size: PhysicalSize<u32>) {
        let scale = self.window.scale_factor();
        let size = self.window.outer_size();
        let (w, h) = (size.width as f64, size.height as f64);
        let gap = GAP * scale;
        let cx = anchor_pos.x + anchor_size.width as f64 / 2.0;
        let (ax, ay) = (anchor_pos.x as i32, anchor_pos.y as i32);
        let work = work_area(ax, ay);
        let (left, top, right, bottom) = (work.0 as f64, work.1 as f64, work.2 as f64, work.3 as f64);
        let x = (cx - w / 2.0).clamp(left + gap, (right - w - gap).max(left + gap));
        // Taskbar at the bottom (usual), top, or a side.
        let y = if anchor_pos.y >= bottom - 2.0 {
            bottom - h - gap
        } else if anchor_pos.y + (anchor_size.height as f64) <= top + 2.0 {
            top + gap
        } else {
            (anchor_pos.y - h - gap).max(top + gap)
        };
        self.window.set_outer_position(PhysicalPosition::new(x, y));
    }

    fn apply_theme(&mut self, dark: bool) {
        self.dark = dark;
        let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        let (bg, card, line) = if dark {
            (Color32::from_rgb(0x20, 0x20, 0x20), Color32::from_rgb(0x2b, 0x2b, 0x2b), Color32::from_rgb(0x3a, 0x3a, 0x3a))
        } else {
            (Color32::from_rgb(0xf3, 0xf3, 0xf3), Color32::WHITE, Color32::from_rgb(0xe0, 0xe0, 0xe0))
        };
        v.panel_fill = bg;
        v.window_fill = card;
        v.extreme_bg_color = card;
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        v.hyperlink_color = ACCENT;
        let r = CornerRadius::same(6);
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = r;
        }
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, line);
        // egui's default text is dim; use the Windows text colours.
        let text = if dark { Color32::from_gray(240) } else { Color32::from_gray(26) };
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text);
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, text);
        v.widgets.inactive.weak_bg_fill = card;
        v.widgets.inactive.bg_fill = card;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, line);
        v.window_corner_radius = CornerRadius::same(8);
        v.menu_corner_radius = CornerRadius::same(8);
        let theme = if dark { egui::Theme::Dark } else { egui::Theme::Light };
        self.egui.egui_ctx.set_theme(theme);
        self.egui.egui_ctx.set_visuals_of(theme, v);
        self.egui.egui_ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = Vec2::new(8.0, 6.0);
            s.spacing.button_padding = Vec2::new(10.0, 5.0);
            s.spacing.interact_size.y = 28.0;
            s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
            s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
            s.text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.0));
        });
        self.icon = None;
    }

    pub fn destroy(&mut self) {
        self.egui.destroy();
    }
}

/// Lay out the panel's contents.
fn draw(
    ui: &mut egui::Ui,
    m: &PanelModel,
    icon: Option<egui::TextureId>,
    dark: bool,
    actions: &mut Vec<PanelAction>,
) {
    let weak = if dark { Color32::from_gray(160) } else { Color32::from_gray(96) };
    let semibold = |size: f32| FontId::new(size, FontFamily::Name("semibold".into()));

    // Header: state icon, name and status, on/off switch.
    ui.horizontal(|ui| {
        if let Some(icon) = icon {
            ui.add(egui::Image::new((icon, Vec2::splat(36.0))));
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new("NoiseGate").font(semibold(16.0)));
            let color = if m.warn { WARN } else { weak };
            ui.label(RichText::new(&m.status).color(color));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut on = m.enabled;
            if toggle_switch(ui, &mut on).on_hover_text("Noise removal").changed() {
                actions.push(PanelAction::SetEnabled(on));
            }
        });
    });
    ui.add_space(10.0);

    let section = |ui: &mut egui::Ui, title: &str| {
        ui.add_space(6.0);
        ui.label(RichText::new(title).font(semibold(13.0)).color(weak));
    };

    section(ui, "Microphone");
    combo(ui, "mic", &m.mics, &m.mic, |id| actions.push(PanelAction::PickMic(id)));

    section(ui, "Virtual mic (output)");
    combo(ui, "out", &m.outs, &m.out, |id| actions.push(PanelAction::PickOutput(id)));
    let color = if m.warn { WARN } else { weak };
    ui.label(RichText::new(&m.hint).small().color(color));
    if let Some(b) = m.hint_button {
        if ui.button(b).clicked() {
            actions.push(PanelAction::HintButton);
        }
    }

    section(ui, "Noise suppression");
    let models: Vec<(&'static str, &str)> = m.models.iter().map(|(k, l)| (*k, l.as_str())).collect();
    if let Some(k) = segmented(ui, "model", &models, |k| *k == m.model.as_str()) {
        actions.push(PanelAction::PickModel(k));
    }
    ui.add_space(2.0);
    ui.add_enabled_ui(m.uses_strength, |ui| {
        let strengths: Vec<(f32, &str)> = m.strengths.iter().map(|(d, l)| (*d, *l)).collect();
        if let Some(db) = segmented(ui, "strength", &strengths, |d| (d - m.strength).abs() < 0.5) {
            actions.push(PanelAction::PickStrength(db));
        }
    });
    let note = if m.uses_strength {
        "Strength limits how far noise is pushed down. Natural keeps some room sound."
    } else {
        "Strength applies to DeepFilterNet3 only."
    };
    ui.label(RichText::new(note).small().color(weak));

    ui.add_space(10.0);
    ui.separator();
    ui.horizontal(|ui| {
        let mut auto = m.autostart;
        if ui.checkbox(&mut auto, "Start with Windows").changed() {
            actions.push(PanelAction::SetAutostart(auto));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(cpu) = m.cpu_percent {
                ui.label(RichText::new(format!("CPU {cpu:.1}%")).small().color(weak));
            }
        });
    });
    ui.horizontal(|ui| {
        if ui.button("Refresh devices").clicked() {
            actions.push(PanelAction::Refresh);
        }
        if ui.button("Logs").clicked() {
            actions.push(PanelAction::OpenLogs);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Quit").clicked() {
                actions.push(PanelAction::Quit);
            }
        });
    });
}

/// Full-width dropdown over (id, label) pairs; calls `pick` on a change.
fn combo(ui: &mut egui::Ui, salt: &str, items: &[(String, String)], selected: &str, mut pick: impl FnMut(String)) {
    let current = items
        .iter()
        .find(|(id, _)| id == selected)
        .map(|(_, l)| l.as_str())
        .unwrap_or("—");
    let width = ui.available_width();
    egui::ComboBox::from_id_salt(salt)
        .width(width)
        .selected_text(current)
        .show_ui(ui, |ui| {
            for (id, label) in items {
                if ui.selectable_label(id == selected, label).clicked() && id != selected {
                    pick(id.clone());
                }
            }
        });
}

/// A row of equal-width buttons, one of them selected. Returns the clicked
/// value if it wasn't already selected.
fn segmented<T: Copy>(
    ui: &mut egui::Ui,
    salt: &str,
    items: &[(T, &str)],
    is_selected: impl Fn(&T) -> bool,
) -> Option<T> {
    let mut picked = None;
    ui.push_id(salt, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let n = items.len().max(1) as f32;
            let w = (ui.available_width() - 4.0 * (n - 1.0)) / n;
            for (value, label) in items {
                let sel = is_selected(value);
                let text = if sel {
                    RichText::new(*label).color(Color32::WHITE)
                } else {
                    RichText::new(*label)
                };
                let mut b = egui::Button::new(text).min_size(Vec2::new(w, 30.0));
                if sel {
                    b = b.fill(ACCENT).stroke(Stroke::NONE);
                }
                if ui.add(b).clicked() && !sel {
                    picked = Some(*value);
                }
            }
        });
    });
    picked
}

/// iOS/Windows-style on/off switch.
fn toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = Vec2::new(44.0, 22.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        let off_stroke = ui.visuals().widgets.inactive.fg_stroke.color;
        let fill = if *on { ACCENT } else { Color32::TRANSPARENT };
        let stroke = if *on { Stroke::NONE } else { Stroke::new(1.5, off_stroke) };
        let radius = rect.height() / 2.0;
        ui.painter().rect(rect, CornerRadius::same(radius as u8), fill, stroke, StrokeKind::Inside);
        let knob_r = if response.hovered() { radius * 0.62 } else { radius * 0.55 };
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        let knob = if *on { Color32::WHITE } else { off_stroke };
        ui.painter().circle_filled(egui::pos2(x, rect.center().y), knob_r, knob);
    }
    response
}

/// Use Segoe UI (the Windows UI font) when available; egui's own fonts stay
/// as fallback for symbols.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let dir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let load = |file: &str| std::fs::read(format!("{dir}\\Fonts\\{file}")).ok();
    if let Some(regular) = load("segoeui.ttf") {
        fonts
            .font_data
            .insert("segoe".into(), Arc::new(egui::FontData::from_owned(regular)));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "segoe".into());
    }
    let semibold = match load("seguisb.ttf") {
        Some(b) => {
            fonts
                .font_data
                .insert("segoe-semibold".into(), Arc::new(egui::FontData::from_owned(b)));
            "segoe-semibold"
        }
        None => "segoe",
    };
    let mut family = vec![semibold.to_string()];
    family.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    family.retain(|f| fonts.font_data.contains_key(f));
    fonts.families.insert(FontFamily::Name("semibold".into()), family);
    ctx.set_fonts(fonts);
}

fn hwnd(window: &Window) -> Option<windows::Win32::Foundation::HWND> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(windows::Win32::Foundation::HWND(h.hwnd.get() as _)),
        _ => None,
    }
}

/// Windows 11 rounded corners (no-op on Windows 10).
fn round_corners(window: &Window) {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
        DWM_WINDOW_CORNER_PREFERENCE,
    };
    if let Some(h) = hwnd(window) {
        unsafe {
            let pref = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                h,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&pref as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
                std::mem::size_of_val(&pref) as u32,
            );
        }
    }
}

/// Hide or reveal a window without unmapping it (DWM cloaking).
fn cloak(window: &Window, on: bool) {
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK};
    if let Some(h) = hwnd(window) {
        let value: i32 = on as i32;
        unsafe {
            let _ = DwmSetWindowAttribute(
                h,
                DWMWA_CLOAK,
                (&value as *const i32).cast(),
                std::mem::size_of::<i32>() as u32,
            );
        }
    }
}

/// Make sure the panel gets keyboard focus (and so a focus-lost event when
/// the user clicks elsewhere). Clicking our tray icon lets us take the
/// foreground; this just makes it explicit.
fn force_foreground(window: &Window) {
    use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    if let Some(h) = hwnd(window) {
        unsafe {
            let _ = SetForegroundWindow(h);
        }
    }
}

/// Work area (screen minus taskbar) of the monitor containing (x, y), in
/// physical pixels: (left, top, right, bottom).
fn work_area(x: i32, y: i32) -> (i32, i32, i32, i32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(mon, &mut info).as_bool() {
            let r = info.rcWork;
            (r.left, r.top, r.right, r.bottom)
        } else {
            (0, 0, 1920, 1040)
        }
    }
}
