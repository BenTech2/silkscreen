//! The Silkscreen window: one winit + wgpu window that is the home screen
//! (pair, discover, connect, debug) and, in display mode, the wireless
//! monitor itself — video full-screen with egui drawn on top when needed.

mod render;
mod ui;

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use silkscreen_codec::Picture;
use silkscreen_platform::SourceKind;
use silkscreen_platform::power::WakeLock;
use silkscreen_proto::{DEFAULT_DATAGRAM_LEN, InputEvent, MouseButton, StreamConfig, TouchPhase};
use tracing::{info, warn};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::ctx::{self, Ctx, KeepAwake};
use crate::discovery::Browser;
use crate::display::{DecodingSink, DisplayOptions, PictureSink, caps_for};
use crate::host::{HostHandle, HostOptions};
use crate::keymap;
use render::{Renderer, UiFrame, VideoUpload};
use silkscreen_core::display::DisplayRole;
use ui::{Action, SourceChoice, UiState, View};

#[derive(Debug, Clone)]
pub struct GuiOptions {
    /// Go straight into display mode (`silkscreen display`).
    pub start_display: bool,
    pub display: DisplayOptions,
    /// Display mode in a window instead of fullscreen.
    pub windowed: bool,
    /// Monitor index for display mode (default: the one the window is on).
    pub monitor: Option<usize>,
    pub vsync: bool,
    pub forward_mouse: bool,
    pub forward_keyboard: bool,
    /// Connect to this display (name or address) as soon as the app opens.
    pub connect: Option<String>,
}

pub fn run(opts: GuiOptions, ctx: Arc<Ctx>) -> Result<()> {
    let event_loop = EventLoop::<UiEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    {
        let p = Mutex::new(proxy.clone());
        ctx::set_waker(move || {
            let _ = p.lock().unwrap().send_event(UiEvent::Wake);
        });
    }
    let mut app = App::new(opts, ctx, proxy);
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[derive(Debug)]
enum UiEvent {
    /// Some state changed; redraw the interface if it is visible.
    Wake,
    NewFrame,
    StreamStarted,
    StreamStopped,
}

struct WindowSink {
    video: Arc<VideoUpload>,
    proxy: Mutex<EventLoopProxy<UiEvent>>,
}

impl PictureSink for WindowSink {
    fn stream_started(&self, _cfg: &StreamConfig, _host: &str) {
        let _ = self
            .proxy
            .lock()
            .unwrap()
            .send_event(UiEvent::StreamStarted);
    }

    fn present(&self, pic: &Picture<'_>, _first_packet_at: Instant) {
        // Only wake the UI if it has drawn the previous frame; otherwise the
        // pending redraw will pick up this newer picture anyway.
        let wake = !self.video.dirty.load(Ordering::Acquire);
        self.video.upload(pic);
        if wake {
            let _ = self.proxy.lock().unwrap().send_event(UiEvent::NewFrame);
        }
    }

    fn stream_stopped(&self) {
        self.video.clear();
        let _ = self
            .proxy
            .lock()
            .unwrap()
            .send_event(UiEvent::StreamStopped);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hotkey {
    Disconnect,
    Debug,
    Fullscreen,
}

struct Gfx {
    window: Arc<Window>,
    renderer: Renderer,
    egui: egui_winit::State,
}

struct App {
    ctx: Arc<Ctx>,
    opts: GuiOptions,
    proxy: EventLoopProxy<UiEvent>,
    gfx: Option<Gfx>,
    egui: egui::Context,
    ui: UiState,
    browser: Option<Browser>,
    host: Option<HostHandle>,
    display: Option<DisplayRole>,
    /// A host is streaming to this display.
    streaming: bool,
    overlay: bool,
    hint_until: Option<Instant>,
    modifiers: ModifiersState,
    /// evdev codes forwarded as pressed, so they can be released.
    keys_down: BTreeSet<u16>,
    buttons_down: Vec<MouseButton>,
    /// Windows synthesises mouse events from touch; ignore those briefly.
    last_touch: Option<Instant>,
    repaint_at: Option<Instant>,
    /// "Start display mode" was clicked; needs the event loop to run.
    pending_start: bool,
    /// Held while the display should not sleep (see [`KeepAwake`]).
    wake_lock: Option<WakeLock>,
    /// Acquiring failed this session; don't retry every frame.
    wake_lock_failed: bool,
    last_frame: Option<Instant>,
    local_ips: Vec<IpAddr>,
}

impl App {
    fn new(opts: GuiOptions, ctx: Arc<Ctx>, proxy: EventLoopProxy<UiEvent>) -> Self {
        let egui = egui::Context::default();
        ui::style(&egui);
        App {
            ui: UiState::new(ctx.name(), opts.windowed),
            ctx,
            opts,
            proxy,
            gfx: None,
            egui,
            browser: None,
            host: None,
            display: None,
            streaming: false,
            overlay: false,
            hint_until: None,
            modifiers: ModifiersState::empty(),
            keys_down: BTreeSet::new(),
            buttons_down: Vec::new(),
            last_touch: None,
            repaint_at: None,
            pending_start: false,
            wake_lock: None,
            wake_lock_failed: false,
            last_frame: None,
            local_ips: local_ips(),
        }
    }

    fn init(&mut self, el: &ActiveEventLoop) -> Result<()> {
        let attrs = Window::default_attributes()
            .with_title("Silkscreen")
            .with_inner_size(winit::dpi::LogicalSize::new(1180.0, 760.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(640.0, 420.0));
        let window = Arc::new(el.create_window(attrs)?);
        let renderer = Renderer::new(window.clone(), self.opts.vsync)?;
        let egui = egui_winit::State::new(
            self.egui.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            Some(renderer.max_texture_side()),
        );
        self.gfx = Some(Gfx {
            window,
            renderer,
            egui,
        });
        match Browser::start() {
            Ok(b) => self.browser = Some(b),
            Err(e) => warn!("LAN discovery unavailable: {e:#}"),
        }
        if self.opts.start_display {
            self.start_display(el)?;
        }
        if let Some(target) = self.opts.connect.clone() {
            self.connect(target);
        }
        self.redraw();
        Ok(())
    }

    fn redraw(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    /// The interface is visible (not a bare video frame).
    fn ui_visible(&self) -> bool {
        !self.streaming || self.overlay || self.hint_until.is_some_and(|t| Instant::now() < t)
    }

    // -- display mode -------------------------------------------------------

    fn start_display(&mut self, el: &ActiveEventLoop) -> Result<()> {
        if self.display.is_some() {
            return Ok(());
        }
        let g = self.gfx.as_ref().context("no window")?;
        let monitor = match self.opts.monitor {
            Some(i) => el
                .available_monitors()
                .nth(i)
                .context("no monitor with that index")?,
            // Wayland only knows a window's monitor once it is shown, and
            // has no "primary" monitor; fall back to the first one.
            None => g
                .window
                .current_monitor()
                .or_else(|| el.primary_monitor())
                .or_else(|| el.available_monitors().next())
                .context("no monitors")?,
        };
        let size = monitor.size();
        let refresh_mhz = monitor.refresh_rate_millihertz().unwrap_or(60_000);
        let scale = monitor.scale_factor() as f32;
        info!(
            "display mode on {}: {}x{} @ {:.2} Hz, scale {scale}",
            monitor.name().unwrap_or_default(),
            size.width,
            size.height,
            refresh_mhz as f32 / 1000.0
        );
        let sink = Arc::new(WindowSink {
            video: g.renderer.video.clone(),
            proxy: Mutex::new(self.proxy.clone()),
        });
        let caps = caps_for(
            &self.ctx,
            &self.opts.display,
            size.width,
            size.height,
            refresh_mhz,
            scale,
        );
        let d = &self.opts.display;
        let role = DisplayRole::start(
            d.port,
            d.fixed_code.clone(),
            self.ctx.clone(),
            caps,
            DecodingSink::new(sink, d.hw_decode),
        )?;
        if !self.ui.display_windowed {
            g.window
                .set_fullscreen(Some(Fullscreen::Borderless(Some(monitor))));
        }
        g.window
            .set_title(&format!("Silkscreen – {} (display)", self.ctx.name()));
        self.display = Some(role);
        Ok(())
    }

    /// When "while the picture is changing" stops keeping the screen awake.
    fn idle_deadline(&self) -> Option<Instant> {
        let s = self.ctx.settings();
        (s.keep_awake == KeepAwake::WhileActive)
            .then(|| {
                self.last_frame
                    .map(|t| t + Duration::from_secs(s.idle_minutes as u64 * 60))
            })
            .flatten()
    }

    /// Takes or releases the wake lock to match the setting and the session.
    fn update_wake_lock(&mut self) {
        let want = self.streaming
            && match self.ctx.settings().keep_awake {
                KeepAwake::Never => false,
                KeepAwake::WhileConnected => true,
                KeepAwake::WhileActive => self.idle_deadline().is_some_and(|d| d > Instant::now()),
            };
        if want && self.wake_lock.is_none() && !self.wake_lock_failed {
            match WakeLock::acquire("Showing another computer's screen") {
                Ok(lock) => {
                    info!("keeping this screen awake");
                    self.wake_lock = Some(lock);
                }
                Err(e) => {
                    warn!("cannot keep the screen awake: {e:#}");
                    self.wake_lock_failed = true;
                }
            }
        } else if !want && self.wake_lock.take().is_some() {
            info!("this screen may sleep again");
        }
    }

    fn stop_display(&mut self) {
        self.release_input();
        self.display = None; // stops the service and unadvertises
        self.streaming = false;
        if let Some(g) = &self.gfx {
            g.window.set_fullscreen(None);
            g.window.set_cursor_visible(true);
            g.window.set_title("Silkscreen");
            g.renderer.video.clear();
        }
    }

    /// The disconnect hotkey: end the session (the display stays ready for
    /// the next connection); outside a session, leave display mode.
    fn disconnect_hotkey(&mut self) {
        let Some(role) = &self.display else { return };
        let in_session = self.streaming
            || role.service.state.get().phase != crate::status::DisplayPhase::Waiting;
        if in_session {
            self.release_input();
            let role = self.display.as_ref().unwrap();
            role.service.disconnect();
            info!("disconnected by hotkey");
        } else {
            self.stop_display();
        }
    }

    fn toggle_fullscreen(&self) {
        if let Some(g) = &self.gfx {
            let next = match g.window.fullscreen() {
                Some(_) => None,
                None => Some(Fullscreen::Borderless(g.window.current_monitor())),
            };
            g.window.set_fullscreen(next);
        }
    }

    // -- input forwarding ---------------------------------------------------

    fn send(&self, ev: InputEvent) {
        if self.streaming
            && let Some(d) = &self.display
        {
            d.service.send_input(ev);
        }
    }

    /// Releases every key and button the host thinks is held.
    fn release_input(&mut self) {
        for code in std::mem::take(&mut self.keys_down) {
            self.send(InputEvent::Key {
                evdev_code: code,
                pressed: false,
            });
        }
        for button in std::mem::take(&mut self.buttons_down) {
            self.send(InputEvent::MouseButton {
                button,
                pressed: false,
            });
        }
    }

    fn hotkey(&self, code: KeyCode) -> Option<Hotkey> {
        let m = self.modifiers;
        // Ctrl+Alt+Shift (⌃⌥⇧ on a Mac), or Cmd/Win+Alt+Shift (⌘⌥⇧).
        if !(m.shift_key() && m.alt_key() && (m.control_key() || m.super_key())) {
            return None;
        }
        match code {
            KeyCode::KeyG => Some(Hotkey::Disconnect),
            KeyCode::KeyD => Some(Hotkey::Debug),
            KeyCode::KeyF => Some(Hotkey::Fullscreen),
            _ => None,
        }
    }

    fn mouse_suppressed(&self) -> bool {
        !self.opts.forward_mouse
            || self
                .last_touch
                .is_some_and(|t| t.elapsed() < Duration::from_millis(500))
    }

    /// Handles input while a host is streaming. Returns true if consumed.
    fn forward(&mut self, event: &WindowEvent) -> bool {
        let Some(g) = &self.gfx else { return false };
        let rect = g.renderer.rect;
        match event {
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return true;
                };
                if !self.opts.forward_keyboard || event.repeat {
                    return true; // the host's OS does its own key repeat
                }
                let Some(evdev) = keymap::evdev(code) else {
                    return true;
                };
                let pressed = event.state == ElementState::Pressed;
                if pressed {
                    self.keys_down.insert(evdev);
                } else if !self.keys_down.remove(&evdev) {
                    return true; // never sent the press (e.g. hotkey)
                }
                self.send(InputEvent::Key {
                    evdev_code: evdev,
                    pressed,
                });
            }
            WindowEvent::Focused(false) => self.release_input(),
            WindowEvent::Touch(t) => {
                self.last_touch = Some(Instant::now());
                let (x, y) = rect.normalise_clamped(t.location.x, t.location.y);
                let phase = match t.phase {
                    winit::event::TouchPhase::Started => TouchPhase::Down,
                    winit::event::TouchPhase::Moved => TouchPhase::Move,
                    winit::event::TouchPhase::Ended => TouchPhase::Up,
                    winit::event::TouchPhase::Cancelled => TouchPhase::Cancel,
                };
                let pressure = t.force.map(|f| f.normalized() as f32);
                self.send(InputEvent::Touch {
                    id: t.id as u32,
                    phase,
                    x,
                    y,
                    pressure,
                });
            }
            WindowEvent::CursorMoved { position, .. } if !self.mouse_suppressed() => {
                if let Some((x, y)) = rect.normalise(position.x, position.y) {
                    self.send(InputEvent::MouseMove { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } if !self.mouse_suppressed() => {
                let button = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    winit::event::MouseButton::Back => MouseButton::Back,
                    winit::event::MouseButton::Forward => MouseButton::Forward,
                    winit::event::MouseButton::Other(_) => return true,
                };
                let pressed = *state == ElementState::Pressed;
                if pressed {
                    self.buttons_down.push(button);
                } else {
                    self.buttons_down.retain(|b| *b != button);
                }
                self.send(InputEvent::MouseButton { button, pressed });
            }
            WindowEvent::MouseWheel { delta, .. } if !self.mouse_suppressed() => {
                self.send(match *delta {
                    MouseScrollDelta::LineDelta(dx, dy) => InputEvent::Scroll {
                        dx,
                        dy,
                        pixels: false,
                    },
                    MouseScrollDelta::PixelDelta(p) => InputEvent::Scroll {
                        dx: p.x as f32,
                        dy: p.y as f32,
                        pixels: true,
                    },
                });
            }
            _ => return false,
        }
        true
    }

    // -- UI -----------------------------------------------------------------

    fn draw(&mut self) {
        let show_hint = self.hint_until.is_some_and(|t| Instant::now() < t);
        let ui_frame = if self.ui_visible() {
            let Some(g) = &mut self.gfx else { return };
            let raw = g.egui.take_egui_input(&g.window);
            let view = View {
                ctx: &self.ctx,
                displays: self
                    .browser
                    .as_ref()
                    .map(|b| b.displays())
                    .unwrap_or_default(),
                host: self.host.as_ref().map(|h| h.state.get()),
                display: self.display.as_ref().map(|d| d.service.state.get()),
                local_ips: &self.local_ips,
                monitor: monitor_desc(&g.window),
                overlay: self.overlay,
                awake: self.wake_lock.is_some(),
            };
            let mut actions = Vec::new();
            let st = &mut self.ui;
            let in_display = self.display.is_some();
            let streaming = self.streaming;
            let out = self.egui.run_ui(raw, |ui| {
                if streaming {
                    ui::stream_overlay(ui.ctx(), st, &view, show_hint);
                } else if in_display {
                    ui::display_waiting(ui, st, &view, &mut actions);
                } else {
                    ui::home(ui, st, &view, &mut actions);
                }
            });
            g.egui
                .handle_platform_output(&g.window, out.platform_output);
            self.repaint_at = out
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .map(|v| v.repaint_delay)
                .filter(|d| *d < Duration::from_secs(5))
                .map(|d| Instant::now() + d);
            if show_hint {
                // Redraw once more when the hint expires.
                self.repaint_at = self.repaint_at.min(self.hint_until).or(self.hint_until);
            }
            let ppp = out.pixels_per_point;
            let primitives = self.egui.tessellate(out.shapes, ppp);
            self.apply(actions);
            Some(UiFrame {
                primitives,
                textures: out.textures_delta,
                pixels_per_point: ppp,
            })
        } else {
            None
        };
        if let Some(g) = &mut self.gfx {
            g.renderer.render(ui_frame);
        }
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                // Needs the event loop; deferred via a flag picked up in `user_event`.
                Action::StartDisplay => self.pending_start = true,
                Action::StopDisplay => self.stop_display(),
                Action::Connect(target) => self.connect(target),
                Action::SubmitCode(code) => {
                    if let Some(h) = &self.host {
                        h.submit_code(&code);
                    }
                }
                Action::StopHost => {
                    if let Some(h) = &self.host {
                        h.stop();
                    }
                }
                Action::DismissHost => self.host = None,
                Action::Forget(key) => self.ctx.paired.remove(&key),
                Action::Rename(name) => {
                    self.ctx.set_name(&name);
                    self.ui.name_edit = self.ctx.name();
                    info!("renamed this computer to \"{}\"", self.ctx.name());
                }
                Action::ToggleOverlay => self.overlay = !self.overlay,
                Action::SetKeepAwake(mode, minutes) => {
                    self.ctx.update_settings(|s| {
                        s.keep_awake = mode;
                        s.idle_minutes = minutes.max(1);
                    });
                    self.update_wake_lock();
                }
            }
        }
        if self.pending_start {
            let _ = self.proxy.send_event(UiEvent::Wake);
        }
    }

    fn connect(&mut self, target: String) {
        if self.host.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        let st = &self.ui;
        let opts = HostOptions {
            target: Some(target),
            codec: st.codec,
            bitrate_kbps: (st.bitrate_mbps > 0.0).then_some((st.bitrate_mbps * 1000.0) as u32),
            encoder: None,
            source: match st.source {
                SourceChoice::Virtual => SourceKind::Virtual,
                SourceChoice::Monitor => SourceKind::Monitor(
                    Some(st.monitor_name.trim().to_owned()).filter(|s| !s.is_empty()),
                ),
                SourceChoice::TestPattern => SourceKind::TestPattern,
            },
            size: None,
            refresh_hz: None,
            datagram_len: DEFAULT_DATAGRAM_LEN,
            persist: true,
            resolution: st.resolution,
        };
        self.host = Some(HostHandle::spawn(opts, self.ctx.clone()));
    }
}

fn monitor_desc(window: &Window) -> String {
    let Some(m) = window.current_monitor() else {
        return String::new();
    };
    let s = m.size();
    let touch = match silkscreen_platform::touch_points() {
        0 => String::new(),
        n => format!(", {n}-point touch"),
    };
    format!(
        "This screen: {}×{} @ {:.0} Hz, scale {}{touch}",
        s.width,
        s.height,
        m.refresh_rate_millihertz().unwrap_or(60_000) as f32 / 1000.0,
        m.scale_factor(),
    )
}

/// Local addresses worth showing on the waiting screen.
fn local_ips() -> Vec<IpAddr> {
    crate::net::local_ip().into_iter().collect()
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_none()
            && let Err(e) = self.init(el)
        {
            warn!("failed to start: {e:#}");
            el.exit();
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UiEvent) {
        if std::mem::take(&mut self.pending_start)
            && let Err(e) = self.start_display(el)
        {
            warn!("cannot start display mode: {e:#}");
        }
        match ev {
            UiEvent::Wake => {
                if self.ui_visible() {
                    self.redraw();
                }
            }
            UiEvent::NewFrame => {
                self.last_frame = Some(Instant::now());
                if self.wake_lock.is_none() {
                    self.update_wake_lock();
                }
                self.redraw();
            }
            UiEvent::StreamStarted => {
                self.streaming = true;
                self.wake_lock_failed = false;
                self.last_frame = Some(Instant::now());
                self.update_wake_lock();
                self.hint_until = Some(Instant::now() + Duration::from_secs(5));
                if let Some(g) = &self.gfx {
                    g.window.set_cursor_visible(false);
                    g.window.focus_window();
                }
                self.redraw();
            }
            UiEvent::StreamStopped => {
                self.release_input();
                self.streaming = false;
                self.update_wake_lock();
                if let Some(g) = &self.gfx {
                    g.window.set_cursor_visible(true);
                }
                self.redraw();
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // With "while the picture is changing", wake up when the idle time
        // runs out to let the screen sleep.
        if self.wake_lock.is_some()
            && let Some(deadline) = self.idle_deadline()
        {
            if deadline <= Instant::now() {
                self.update_wake_lock();
            } else {
                self.repaint_at = Some(self.repaint_at.map_or(deadline, |t| t.min(deadline)));
            }
        }
        match self.repaint_at {
            Some(t) if t <= Instant::now() => {
                self.repaint_at = None;
                self.redraw();
                el.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match &event {
            WindowEvent::CloseRequested => {
                self.release_input();
                el.exit();
                return;
            }
            WindowEvent::Resized(size) => {
                if let Some(g) = &mut self.gfx {
                    g.renderer.resize(size.width, size.height);
                }
                self.redraw();
            }
            WindowEvent::RedrawRequested => {
                self.draw();
                return;
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            _ => {}
        }

        // Hotkeys work in display mode whether or not a host is streaming,
        // and are never forwarded.
        if self.display.is_some()
            && let WindowEvent::KeyboardInput { event: k, .. } = &event
            && let PhysicalKey::Code(code) = k.physical_key
            && let Some(hk) = self.hotkey(code)
        {
            if k.state == ElementState::Pressed && !k.repeat {
                match hk {
                    Hotkey::Disconnect => self.disconnect_hotkey(),
                    Hotkey::Debug => self.overlay = !self.overlay,
                    Hotkey::Fullscreen => self.toggle_fullscreen(),
                }
                self.redraw();
            }
            return;
        }

        if self.streaming && self.forward(&event) {
            return;
        }
        if let Some(g) = &mut self.gfx {
            let r = g.egui.on_window_event(&g.window, &event);
            if r.repaint {
                g.window.request_redraw();
            }
        }
    }
}
