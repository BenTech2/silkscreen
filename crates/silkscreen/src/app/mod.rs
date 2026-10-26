//! The Silkscreen window: one winit + wgpu window that is the home screen
//! (pair, discover, connect, debug) and, in display mode, the wireless
//! monitor itself — video full-screen with egui drawn on top when needed.

mod prefs;
mod render;
mod tray;
mod ui;

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use silkscreen_codec::Picture;
use silkscreen_platform::SourceKind;
use silkscreen_platform::power::WakeLock;
use silkscreen_proto::{InputEvent, MouseButton, StreamConfig, TouchPhase};
use tracing::{Level, info, warn};
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
pub use prefs::Prefs;
use prefs::{LoginStart, SourceChoice, StreamPrefs};
use render::{Renderer, UiFrame, VideoUpload};
use silkscreen_core::display::DisplayRole;
use silkscreen_core::status::{DisplayPhase, HostPhase};
use tray::{Tray, TrayCommand, TrayState};
use ui::{Action, UiState, View};

/// Smallest packet size offered: IPv4's minimum datagram size.
pub const MIN_DATAGRAM_LEN: usize = 576;

#[derive(Debug, Clone)]
pub struct GuiOptions {
    /// Go straight into display mode (`silkscreen display`).
    pub start_display: bool,
    /// Saved settings, with this run's command-line flags applied.
    pub prefs: Prefs,
    /// `--pairing-code`: a fixed code instead of a random one.
    pub fixed_code: Option<String>,
    /// Connect to these displays (names or addresses) as soon as the app opens.
    pub connect: Vec<String>,
    /// Mirror to these displays as soon as the app opens.
    pub mirror: Vec<String>,
    /// From `-v` / `-vv`.
    pub log_level: Level,
    /// Started at login (`--autostart`, see `silkscreen_platform::autostart`).
    pub autostart: bool,
}

pub fn run(opts: GuiOptions, ctx: Arc<Ctx>) -> Result<()> {
    silkscreen_platform::install_app_entry();
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
    Tray(TrayCommand),
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
        // Wake the UI unless a redraw is already on its way.
        if self.video.upload(pic) {
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
    HwDecode,
}

/// Extending this computer to one display.
struct HostSession {
    /// What was connected to (name or address), to recognise the display.
    target: String,
    /// Shows a copy of one of this computer's screens instead of its own
    /// virtual monitor.
    mirror: bool,
    handle: HostHandle,
}

impl HostSession {
    fn active(&self) -> bool {
        !self.handle.is_finished()
    }
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
    /// Host sessions, one per display this computer extends or mirrors to.
    hosts: Vec<HostSession>,
    /// This computer's screens (for Mirror), refreshed now and then.
    screens: Vec<silkscreen_platform::MonitorInfo>,
    screens_at: Option<Instant>,
    display: Option<DisplayRole>,
    /// The display's decoder, to switch hardware decoding mid-stream.
    decoder: Option<Arc<DecodingSink>>,
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
    /// The settings as last saved (or as given), to notice changes.
    saved_prefs: Prefs,
    /// The stream options the running host was given.
    host_stream: StreamPrefs,
    /// When stream options last changed, to apply them once they settle.
    stream_edited: Option<Instant>,
    /// The level being logged, which the Debug tab can raise.
    log_level: Level,
    tray: Option<Tray>,
    /// The window is closed to the tray.
    hidden: bool,
    /// The OS says the window can't be seen (covered, minimised, locked).
    occluded: bool,
}

impl App {
    fn new(opts: GuiOptions, ctx: Arc<Ctx>, proxy: EventLoopProxy<UiEvent>) -> Self {
        let egui = egui::Context::default();
        ui::style(&egui);
        App {
            ui: UiState::new(ctx.name(), opts.prefs.clone(), opts.log_level),
            saved_prefs: opts.prefs.clone(),
            host_stream: opts.prefs.stream.clone(),
            stream_edited: None,
            log_level: opts.log_level,
            tray: None,
            hidden: false,
            occluded: false,
            ctx,
            opts,
            proxy,
            gfx: None,
            egui,
            browser: None,
            hosts: Vec::new(),
            screens: Vec::new(),
            screens_at: None,
            display: None,
            decoder: None,
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
        let app = self.ui.prefs.app.clone();
        let login = self.opts.autostart.then_some(app.login_start);
        // Starting in the tray needs the tray icon to get back to the window.
        self.hidden = login == Some(LoginStart::Tray) && app.tray;
        let attrs = Window::default_attributes()
            .with_visible(!self.hidden)
            .with_title("Silkscreen")
            .with_inner_size(winit::dpi::LogicalSize::new(1180.0, 760.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(640.0, 420.0))
            .with_window_icon(window_icon());
        // Wayland compositors take the icon from the desktop entry with this
        // app id (installed by the platform layer), not from the window.
        #[cfg(target_os = "linux")]
        let attrs = winit::platform::wayland::WindowAttributesExtWayland::with_name(
            attrs,
            "silkscreen",
            "silkscreen",
        );
        let window = Arc::new(el.create_window(attrs)?);
        let renderer = Renderer::new(window.clone(), self.ui.prefs.display.vsync)?;
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
        set_app_icon();
        self.set_tray(app.tray);
        if self.hidden {
            set_dock_icon(false);
        }
        if app.start_at_login {
            // Refresh the login entry, in case the app moved.
            set_start_at_login(true);
        }
        // The login choice says full screen or windowed itself, whatever
        // the "Run in a window" display option says.
        match login {
            Some(LoginStart::Display) => self.start_display(el, false)?,
            Some(LoginStart::DisplayWindowed) => self.start_display(el, true)?,
            _ if self.opts.start_display => {
                self.start_display(el, self.ui.prefs.display.windowed)?
            }
            _ => {}
        }
        for target in self.opts.connect.clone() {
            self.connect(target, false);
        }
        for target in self.opts.mirror.clone() {
            self.connect(target, true);
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

    /// Starts display mode, full screen unless `windowed`.
    fn start_display(&mut self, el: &ActiveEventLoop, windowed: bool) -> Result<()> {
        if self.display.is_some() {
            return Ok(());
        }
        let g = self.gfx.as_ref().context("no window")?;
        let prefs = &self.ui.prefs.display;
        let chosen = prefs.monitor.and_then(|i| {
            let m = el.available_monitors().nth(i);
            if m.is_none() {
                warn!("no monitor {i} (unplugged?); using the window's");
            }
            m
        });
        // Wayland only knows a window's monitor once it is shown, and has no
        // "primary" monitor; fall back to the first one.
        let monitor = chosen
            .or_else(|| g.window.current_monitor())
            .or_else(|| el.primary_monitor())
            .or_else(|| el.available_monitors().next())
            .context("no monitors")?;
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
        let d = DisplayOptions {
            port: prefs.port,
            hw_decode: prefs.hw_decode,
            fixed_code: self.opts.fixed_code.clone(),
            catch_up: catch_up(prefs.skip_behind_ms),
        };
        let caps = caps_for(&self.ctx, &d, size.width, size.height, refresh_mhz, scale);
        let decoder = DecodingSink::new(sink, d.hw_decode);
        let role = DisplayRole::start(
            d.port,
            d.fixed_code.clone(),
            self.ctx.clone(),
            caps,
            decoder.clone(),
        )?;
        self.decoder = Some(decoder);
        role.service.set_catch_up(d.catch_up);
        if !windowed {
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

    // -- tray ---------------------------------------------------------------

    fn set_tray(&mut self, on: bool) {
        if on == self.tray.is_some() {
            return;
        }
        if !on {
            self.tray = None;
            if self.hidden {
                self.show_window();
            }
            return;
        }
        let proxy = Mutex::new(self.proxy.clone());
        match Tray::new(move |cmd| {
            let _ = proxy.lock().unwrap().send_event(UiEvent::Tray(cmd));
        }) {
            Ok(t) => {
                self.tray = Some(t);
                self.update_tray();
            }
            Err(e) => warn!("cannot show a tray icon: {e:#}"),
        }
    }

    /// Closes the window to the tray; connections keep running.
    fn hide_window(&mut self) {
        self.release_input();
        // In display mode the window *is* the display.
        if self.display.is_some() {
            self.stop_display();
        }
        if let Some(g) = &self.gfx {
            g.window.set_visible(false);
        }
        self.hidden = true;
        set_dock_icon(false);
        self.update_tray();
    }

    fn show_window(&mut self) {
        set_dock_icon(true);
        if let Some(g) = &self.gfx {
            g.window.set_visible(true);
            g.window.focus_window();
        }
        self.hidden = false;
        self.redraw();
    }

    fn tray_command(&mut self, el: &ActiveEventLoop, cmd: TrayCommand) {
        match cmd {
            TrayCommand::Open => self.show_window(),
            TrayCommand::Disconnect => {
                for s in &self.hosts {
                    s.handle.stop();
                }
                if self.streaming {
                    self.release_input();
                    if let Some(d) = &self.display {
                        d.service.disconnect();
                    }
                }
            }
            TrayCommand::ToggleDisplay => {
                if self.display.is_some() {
                    self.stop_display();
                } else {
                    self.show_window();
                    if let Err(e) = self.start_display(el, self.ui.prefs.display.windowed) {
                        warn!("cannot start display mode: {e:#}");
                    }
                }
            }
            TrayCommand::Quit => {
                self.release_input();
                el.exit();
            }
        }
        self.update_tray();
    }

    /// Shows what's happening in the tray menu and tooltip.
    fn update_tray(&mut self) {
        let hosts: Vec<_> = self.hosts.iter().map(|s| s.handle.state.get()).collect();
        // A pairing code has to be typed into the window.
        if self.hidden
            && hosts
                .iter()
                .any(|h| matches!(h.phase, HostPhase::NeedCode(_)))
        {
            self.show_window();
        }
        let Some(tray) = &mut self.tray else { return };
        let display = self.display.as_ref().map(|d| d.service.state.get());
        let names = |want: fn(&HostPhase) -> bool| {
            let names: Vec<_> = hosts
                .iter()
                .filter(|h| want(&h.phase))
                .map(|h| h.display.clone().unwrap_or_else(|| "a display".into()))
                .collect();
            (!names.is_empty()).then(|| names.join(", "))
        };
        let status = if let Some(n) = names(|p| matches!(p, HostPhase::Streaming)) {
            format!("Extending to {n}")
        } else if let Some(n) = names(|p| matches!(p, HostPhase::NeedCode(_))) {
            format!("Pairing with {n}")
        } else if names(|p| !matches!(p, HostPhase::Stopped(_))).is_some() {
            "Connecting…".into()
        } else {
            match display.as_ref().map(|d| &d.phase) {
                Some(DisplayPhase::Streaming { host }) => format!("Showing {host}'s screen"),
                Some(DisplayPhase::Pairing { host, .. }) => format!("Pairing with {host}"),
                Some(_) => "Display mode: waiting for a host".into(),
                None => "Not connected".into(),
            }
        };
        let hosting = self.hosts.iter().any(HostSession::active);
        tray.update(TrayState {
            status,
            can_disconnect: hosting || self.streaming,
            in_display_mode: self.display.is_some(),
        });
    }

    fn stop_display(&mut self) {
        self.release_input();
        self.display = None; // stops the service and unadvertises
        self.decoder = None;
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
            KeyCode::KeyH => Some(Hotkey::HwDecode),
            _ => None,
        }
    }

    fn mouse_suppressed(&self) -> bool {
        !self.ui.prefs.display.forward_mouse
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
                if !self.ui.prefs.display.forward_keyboard || event.repeat {
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
        if self.hidden || self.occluded {
            return;
        }
        self.refresh_screens(false);
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
                hosts: self
                    .hosts
                    .iter()
                    .map(|s| (s.target.clone(), s.mirror, s.handle.state.get()))
                    .collect(),
                screens: &self.screens,
                display: self.display.as_ref().map(|d| {
                    let mut s = d.service.state.get();
                    // The decoder can change mid-stream (switch, fallback).
                    if let Some(name) = self.decoder.as_ref().and_then(|d| d.decoder_name()) {
                        s.decoder = Some(name);
                    }
                    s
                }),
                local_ips: &self.local_ips,
                monitor: monitor_desc(&g.window),
                monitors: monitor_labels(&g.window),
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
            self.sync_settings();
            self.update_tray();
            Some(UiFrame {
                primitives,
                textures: out.textures_delta,
                pixels_per_point: ppp,
            })
        } else {
            None
        };
        if let Some(g) = &mut self.gfx
            && !g.renderer.render(ui_frame)
        {
            // Hidden without an Occluded event (some systems never send
            // one): look again now and then rather than spinning.
            self.repaint_at = Some(Instant::now() + Duration::from_millis(250));
        }
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                // Needs the event loop; deferred via a flag picked up in `user_event`.
                Action::StartDisplay => self.pending_start = true,
                Action::StopDisplay => self.stop_display(),
                Action::Connect(target, mirror) => self.connect(target, mirror),
                Action::SubmitCode(i, code) => {
                    if let Some(h) = self.hosts.get(i).map(|s| &s.handle) {
                        h.submit_code(&code);
                    }
                }
                Action::StopHost(i) => {
                    if let Some(h) = self.hosts.get(i).map(|s| &s.handle) {
                        h.stop();
                    }
                }
                Action::StopAllHosts => {
                    for s in &self.hosts {
                        s.handle.stop();
                    }
                }
                Action::DismissHost(i) => {
                    if i < self.hosts.len() {
                        self.hosts.remove(i);
                    }
                }
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

    /// Saves settings changed in the window and applies those that take
    /// effect immediately.
    fn sync_settings(&mut self) {
        let prefs = &self.ui.prefs;
        if *prefs != self.saved_prefs {
            let (tray, login) = (prefs.app.tray, prefs.app.start_at_login);
            if login != self.saved_prefs.app.start_at_login {
                set_start_at_login(login);
            }
            let (new, old) = (&prefs.display, &self.saved_prefs.display);
            if new.vsync != old.vsync
                && let Some(g) = &mut self.gfx
            {
                g.renderer.set_vsync(new.vsync);
            }
            if new.hw_decode != old.hw_decode
                && let Some(d) = &self.decoder
            {
                d.set_hw_decode(new.hw_decode);
            }
            if new.skip_behind_ms != old.skip_behind_ms
                && let Some(d) = &self.display
            {
                d.service.set_catch_up(catch_up(new.skip_behind_ms));
            }
            if prefs.stream != self.saved_prefs.stream {
                self.stream_edited = Some(Instant::now());
            }
            prefs.save();
            self.saved_prefs = prefs.clone();
            self.set_tray(tray);
        }
        self.update_host();
        // Showing debug or trace lines in the log means recording them.
        let level = self.ui.log_level.max(Level::INFO);
        if level != self.log_level {
            crate::set_log_level(level);
            self.log_level = level;
        }
    }

    /// Hands changed stream options to the running host once they have
    /// settled, so dragging a slider or typing a size doesn't restart the
    /// encoder (or reconnect) on every step.
    fn update_host(&mut self) {
        const SETTLE: Duration = Duration::from_millis(700);
        let Some(edited) = self.stream_edited else {
            return;
        };
        if !self.hosts.iter().any(HostSession::active) || self.ui.prefs.stream == self.host_stream {
            self.stream_edited = None;
            return;
        }
        let due = edited + SETTLE;
        let now = Instant::now();
        if now < due || self.egui.egui_is_using_pointer() {
            let next = due.max(now + Duration::from_millis(100));
            self.repaint_at = Some(self.repaint_at.map_or(next, |t| t.min(next)));
            return;
        }
        for s in self.hosts.iter().filter(|s| s.active()) {
            s.handle.update(self.host_options(None, s.mirror));
        }
        self.host_stream = self.ui.prefs.stream.clone();
        self.stream_edited = None;
    }

    /// Starts extending (or mirroring) to another display, alongside any
    /// others.
    fn connect(&mut self, target: String, mirror: bool) {
        if self.hosts.iter().any(|s| s.target == target && s.active()) {
            return;
        }
        self.hosts.retain(|s| s.target != target);
        self.refresh_screens(true);
        let opts = self.host_options(Some(target.clone()), mirror);
        // Sessions share one set of stream options.
        if !self.hosts.iter().any(HostSession::active) {
            self.host_stream = self.ui.prefs.stream.clone();
            self.stream_edited = None;
        }
        self.hosts.push(HostSession {
            target,
            mirror,
            handle: HostHandle::spawn(opts, self.ctx.clone()),
        });
    }

    /// Re-reads this computer's screens every few seconds (or now).
    fn refresh_screens(&mut self, now: bool) {
        if now
            || self
                .screens_at
                .is_none_or(|t| t.elapsed() > Duration::from_secs(2))
        {
            self.screens = silkscreen_platform::monitors();
            self.screens_at = Some(Instant::now());
        }
    }

    fn host_options(&self, target: Option<String>, mirror: bool) -> HostOptions {
        let st = &self.ui.prefs.stream;
        // The chosen screen, else the main one; with no list (Linux) the
        // system's screen-sharing dialog asks.
        let mirrored = || {
            let main = self.screens.iter().find(|m| m.main).map(|m| m.id.clone());
            SourceKind::Monitor(st.mirror_screen.clone().or(main))
        };
        HostOptions {
            target,
            codec: st.codec,
            bitrate_kbps: (st.bitrate_mbps > 0.0).then_some((st.bitrate_mbps * 1000.0) as u32),
            encoder: st.codec.and(st.encoder.clone()),
            source: match st.source {
                _ if mirror => mirrored(),
                SourceChoice::Virtual => SourceKind::Virtual,
                SourceChoice::Monitor => SourceKind::Monitor(
                    Some(st.monitor_name.trim().to_owned()).filter(|s| !s.is_empty()),
                ),
                SourceChoice::TestPattern => SourceKind::TestPattern,
            },
            size: st.size,
            refresh_hz: st.refresh_hz,
            datagram_len: st.datagram_len.max(MIN_DATAGRAM_LEN),
            persist: st.reconnect,
            resolution: st.resolution,
            max_fps: st.max_fps.map(|f| f as f32),
            chroma444: st.full_color,
        }
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

/// Monitors as listed in the display options, numbered in the system's
/// order (macOS names them only "Monitor #<id>").
fn monitor_labels(window: &Window) -> Vec<String> {
    let primary = window.primary_monitor();
    window
        .available_monitors()
        .enumerate()
        .map(|(i, m)| {
            let s = m.size();
            format!(
                "{}. {} — {}×{}{}",
                i + 1,
                m.name().unwrap_or_else(|| "Monitor".into()),
                s.width,
                s.height,
                if primary.as_ref() == Some(&m) {
                    " (main)"
                } else {
                    ""
                }
            )
        })
        .collect()
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
            && let Err(e) = self.start_display(el, self.ui.prefs.display.windowed)
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
            UiEvent::Tray(cmd) => self.tray_command(el, cmd),
        }
        if !matches!(ev, UiEvent::NewFrame) {
            self.update_tray();
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
                if self.tray.is_some() {
                    self.hide_window();
                } else {
                    self.release_input();
                    el.exit();
                }
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
            WindowEvent::Occluded(o) => {
                self.occluded = *o;
                self.redraw();
            }
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
                    Hotkey::HwDecode => {
                        let d = &mut self.ui.prefs.display;
                        d.hw_decode = !d.hw_decode;
                        info!(
                            "hardware decoding {}",
                            if d.hw_decode { "on" } else { "off" }
                        );
                        self.sync_settings();
                    }
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

fn catch_up(ms: Option<u32>) -> Option<Duration> {
    ms.map(|ms| Duration::from_millis(ms as u64))
}

fn set_start_at_login(on: bool) {
    match silkscreen_platform::autostart::set_enabled(on) {
        Ok(()) => info!(
            "Silkscreen {} start at login",
            if on { "will" } else { "won't" }
        ),
        Err(e) => warn!("cannot change starting at login: {e:#}"),
    }
}

/// Shows or hides the Dock icon: while the window is closed to the menu
/// bar, Silkscreen is a menu bar app.
#[cfg(target_os = "macos")]
fn set_dock_icon(visible: bool) {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    });
    if visible {
        set_app_icon();
        #[allow(deprecated)] // `activate` needs macOS 14
        app.activateIgnoringOtherApps(true);
    }
}

/// The Dock and app switcher icon. Silkscreen.app has it in its bundle, but
/// a plain `silkscreen` binary would show the generic executable icon.
#[cfg(target_os = "macos")]
fn set_app_icon() {
    use objc2::AllocAnyThread;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let png = include_bytes!("../../../../assets/png/macos/icon-512.png");
    let data = NSData::with_bytes(png);
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }
}

#[cfg(not(target_os = "macos"))]
fn set_app_icon() {}

#[cfg(not(target_os = "macos"))]
fn set_dock_icon(_visible: bool) {}

/// The app icon for the title bar and taskbar (64x64 RGBA from assets/).
fn window_icon() -> Option<winit::window::Icon> {
    let rgba = include_bytes!("../../../../assets/icon-64.rgba");
    winit::window::Icon::from_rgba(rgba.to_vec(), 64, 64).ok()
}
