//! KDE Plasma backend: talks to KWin directly instead of going through the
//! portal, which lets us create the virtual monitor at exactly the display's
//! resolution *and* scale, with no dialog — the closest thing to plugging in
//! a monitor.
//!
//! Uses two privileged KWin protocols (the same ones `krfb-virtualmonitor`
//! uses): `zkde_screencast_unstable_v1` for the virtual output + PipeWire
//! stream, and `org_kde_kwin_fake_input` for touch/pointer injection. KWin
//! only offers them to programs whose installed `.desktop` file lists them in
//! `X-KDE-Wayland-Interfaces`; [`ensure_desktop_entry`] installs one.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use silkscreen_proto::{InputEvent, MouseButton, TouchPhase};
use tracing::{debug, info, warn};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_plasma::fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1::{self, ZkdeScreencastStreamUnstableV1},
    zkde_screencast_unstable_v1::{self, ZkdeScreencastUnstableV1},
};

use crate::{InputSink, ScreenRequest};

const SCREENCAST: &str = "zkde_screencast_unstable_v1";
const FAKE_INPUT: &str = "org_kde_kwin_fake_input";

/// Logical-coordinate rectangle of an output in the compositor's space.
#[derive(Debug, Clone, Copy, Default)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[derive(Default)]
struct OutputInfo {
    _wl: Option<wl_output::WlOutput>,
    _xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    rect: Rect,
}

enum StreamEvent {
    Created(u32),
    Failed(String),
}

struct State {
    xdg_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    outputs: HashMap<u32, OutputInfo>,
    stream_event: Option<StreamEvent>,
    closed: Arc<AtomicBool>,
    /// Registry name of our virtual output once it appears.
    ours: Option<u32>,
    geometry: Arc<Mutex<Option<Rect>>>,
}

pub struct KwinScreen {
    pub node_id: u32,
    pub guard: Box<dyn Send>,
    pub input: Box<dyn InputSink>,
    /// Pixel size of the virtual output.
    pub size: (i32, i32),
}

/// Whether KWin will hand us the privileged interfaces.
pub fn available() -> bool {
    let Ok(conn) = Connection::connect_to_env() else {
        return false;
    };
    let Ok((globals, _queue)) = registry_queue_init::<State>(&conn) else {
        return false;
    };
    let names: HashSet<String> = globals
        .contents()
        .clone_list()
        .into_iter()
        .map(|g| g.interface)
        .collect();
    names.contains(SCREENCAST) && names.contains(FAKE_INPUT)
}

pub fn open(req: &ScreenRequest) -> Result<KwinScreen> {
    let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let list = globals.contents().clone_list();

    let screencast: ZkdeScreencastUnstableV1 = globals.bind(&qh, 2..=4, ()).map_err(|_| {
        anyhow!("KWin did not grant {SCREENCAST}; is the .desktop entry installed?")
    })?;
    let fake_input: OrgKdeKwinFakeInput = globals
        .bind(&qh, 4..=5, ())
        .map_err(|_| anyhow!("KWin did not grant {FAKE_INPUT}"))?;
    let closed = Arc::new(AtomicBool::new(false));
    let geometry = Arc::new(Mutex::new(None));
    let mut state = State {
        xdg_manager: globals.bind(&qh, 1..=3, ()).ok(),
        outputs: HashMap::new(),
        stream_event: None,
        closed: closed.clone(),
        ours: None,
        geometry: geometry.clone(),
    };
    for g in list.iter().filter(|g| g.interface == "wl_output") {
        state.bind_output(globals.registry(), &qh, g.name, g.version);
    }
    queue.roundtrip(&mut state)?;
    let before: HashSet<u32> = state.outputs.keys().copied().collect();

    // Despite the protocol calling it "logical", KWin (6.x) uses the size as
    // the output's pixel mode and ignores the scale; the scale is applied
    // afterwards through KScreen.
    let scale = if req.scale > 0.0 {
        req.scale as f64
    } else {
        1.0
    };
    let (lw, lh) = (req.width as i32, req.height as i32);
    let pointer = zkde_screencast_unstable_v1::Pointer::Embedded as u32;
    let stream = if screencast.version() >= 4 {
        screencast.stream_virtual_output_with_description(
            req.name.clone(),
            req.name.clone(),
            lw,
            lh,
            scale,
            pointer,
            &qh,
            (),
        )
    } else {
        screencast.stream_virtual_output(req.name.clone(), lw, lh, scale, pointer, &qh, ())
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    let node_id = loop {
        queue.roundtrip(&mut state)?;
        match state.stream_event.take() {
            Some(StreamEvent::Created(node)) => break node,
            Some(StreamEvent::Failed(e)) => bail!("KWin could not create the virtual output: {e}"),
            None if Instant::now() > deadline => bail!("KWin did not create the virtual output"),
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };

    // The new virtual output shows up as a new wl_output global.
    let deadline = Instant::now() + Duration::from_secs(2);
    while state.ours.is_none() && Instant::now() < deadline {
        queue.roundtrip(&mut state)?;
        state.ours = state.outputs.keys().copied().find(|k| !before.contains(k));
        std::thread::sleep(Duration::from_millis(10));
    }
    queue.roundtrip(&mut state)?;
    state.publish_geometry();
    match *geometry.lock().unwrap() {
        Some(r) => info!(
            "virtual output at {},{} ({}x{} logical before scaling to {scale}x)",
            r.x, r.y, r.w, r.h
        ),
        None => warn!("could not locate the virtual output; touch positions may be wrong"),
    }

    // Always set it: KWin remembers each output's last scale by name.
    set_scale(&format!("Virtual-{}", req.name), scale);

    fake_input.authenticate(
        "Silkscreen".into(),
        "Forward touch and pen input from a wireless display".into(),
    );
    conn.flush()?;

    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("kwin".into())
            .spawn(move || event_loop(queue, state, stop))?
    };

    Ok(KwinScreen {
        node_id,
        guard: Box::new(Guard {
            conn: conn.clone(),
            stream: Some(stream),
            stop,
            thread: Some(thread),
        }),
        input: Box::new(KwinInput {
            conn,
            fake: fake_input,
            geometry,
            closed,
            slots: HashMap::new(),
            pen_down: false,
        }),
        size: (lw, lh),
    })
}

fn event_loop(mut queue: EventQueue<State>, mut state: State, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) && !state.closed.load(Ordering::Relaxed) {
        if let Err(e) = queue.dispatch_pending(&mut state) {
            debug!("wayland dispatch: {e}");
            break;
        }
        let _ = queue.flush();
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        // Wake up regularly to notice `stop`.
        let mut fds = [libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&guard.connection_fd()),
            events: libc::POLLIN,
            revents: 0,
        }];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, 200) };
        if ready > 0 {
            if guard.read().is_err() {
                break;
            }
        } else {
            drop(guard);
        }
        state.publish_geometry();
    }
    state.closed.store(true, Ordering::Relaxed);
}

/// Closing the stream removes the virtual output — "unplugging" the monitor.
struct Guard {
    conn: Connection,
    stream: Option<ZkdeScreencastStreamUnstableV1>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(s) = self.stream.take() {
            s.close();
        }
        let _ = self.conn.flush();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl State {
    fn bind_output(
        &mut self,
        registry: &wl_registry::WlRegistry,
        qh: &QueueHandle<State>,
        name: u32,
        version: u32,
    ) {
        let wl: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
        let xdg = self
            .xdg_manager
            .as_ref()
            .map(|m| m.get_xdg_output(&wl, qh, name));
        self.outputs.insert(
            name,
            OutputInfo {
                _wl: Some(wl),
                _xdg: xdg,
                rect: Rect::default(),
            },
        );
    }

    fn publish_geometry(&self) {
        if let Some(info) = self.ours.and_then(|n| self.outputs.get(&n))
            && info.rect.w > 0.0
        {
            *self.geometry.lock().unwrap() = Some(info.rect);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => {
                state.bind_output(registry, qh, name, version);
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.remove(&name);
                if state.ours == Some(name) {
                    state.closed.store(true, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(info) = state.outputs.get_mut(name) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                (info.rect.x, info.rect.y) = (x as f64, y as f64)
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                (info.rect.w, info.rect.h) = (width as f64, height as f64)
            }
            _ => {}
        }
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        event: zkde_screencast_stream_unstable_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zkde_screencast_stream_unstable_v1::Event::Created { node } => {
                state.stream_event = Some(StreamEvent::Created(node))
            }
            zkde_screencast_stream_unstable_v1::Event::Failed { error } => {
                state.stream_event = Some(StreamEvent::Failed(error))
            }
            zkde_screencast_stream_unstable_v1::Event::Closed => {
                state.closed.store(true, Ordering::Relaxed)
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(State: zxdg_output_manager_v1::ZxdgOutputManagerV1);
wayland_client::delegate_noop!(State: ZkdeScreencastUnstableV1);
wayland_client::delegate_noop!(State: OrgKdeKwinFakeInput);

struct KwinInput {
    conn: Connection,
    fake: OrgKdeKwinFakeInput,
    geometry: Arc<Mutex<Option<Rect>>>,
    closed: Arc<AtomicBool>,
    /// Display touch id -> small KWin touch id.
    slots: HashMap<u32, u32>,
    pen_down: bool,
}

const BTN_LEFT: u32 = 0x110;
const PRESSED: u32 = 1;
const RELEASED: u32 = 0;
/// KWin's axis units per wheel notch.
const AXIS_PER_LINE: f64 = 15.0;

impl InputSink for KwinInput {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            bail!("virtual output closed");
        }
        let Some(r) = *self.geometry.lock().unwrap() else {
            return Ok(());
        };
        let at = |x: f32, y: f32| (r.x + x as f64 * r.w, r.y + y as f64 * r.h);
        let f = &self.fake;
        match *ev {
            InputEvent::Touch {
                id, phase, x, y, ..
            } => {
                let (gx, gy) = at(x, y);
                match phase {
                    TouchPhase::Down => {
                        let slot = (0..)
                            .find(|s| !self.slots.values().any(|v| v == s))
                            .unwrap();
                        self.slots.insert(id, slot);
                        f.touch_down(slot, gx, gy);
                    }
                    TouchPhase::Move => {
                        if let Some(&slot) = self.slots.get(&id) {
                            f.touch_motion(slot, gx, gy);
                        }
                    }
                    TouchPhase::Up => {
                        if let Some(slot) = self.slots.remove(&id) {
                            f.touch_up(slot);
                        }
                    }
                    TouchPhase::Cancel => {
                        self.slots.clear();
                        f.touch_cancel();
                    }
                }
                f.touch_frame();
            }
            // fake_input has no tablet support yet: the pen drives the pointer.
            InputEvent::Pen {
                phase, x, y, hover, ..
            } => {
                let (gx, gy) = at(x, y);
                f.pointer_motion_absolute(gx, gy);
                let down = matches!(phase, TouchPhase::Down | TouchPhase::Move) && !hover;
                if down != self.pen_down {
                    self.pen_down = down;
                    f.button(BTN_LEFT, if down { PRESSED } else { RELEASED });
                }
            }
            InputEvent::MouseMove { x, y } => {
                let (gx, gy) = at(x, y);
                f.pointer_motion_absolute(gx, gy);
            }
            InputEvent::MouseButton { button, pressed } => {
                let code = match button {
                    MouseButton::Left => BTN_LEFT,
                    MouseButton::Right => 0x111,
                    MouseButton::Middle => 0x112,
                    MouseButton::Back => 0x113,
                    MouseButton::Forward => 0x114,
                };
                f.button(code, if pressed { PRESSED } else { RELEASED });
            }
            InputEvent::Scroll { dx, dy, pixels } => {
                let k = if pixels { 1.0 } else { AXIS_PER_LINE };
                // Display: positive = up/left. Wayland axis: positive = down/right.
                if dy != 0.0 {
                    f.axis(0, -dy as f64 * k);
                }
                if dx != 0.0 {
                    f.axis(1, -dx as f64 * k);
                }
            }
            InputEvent::Key {
                evdev_code,
                pressed,
            } => {
                f.keyboard_key(evdev_code as u32, if pressed { PRESSED } else { RELEASED });
            }
        }
        self.conn.flush()?;
        Ok(())
    }
}

/// Sets an output's scale through KScreen (the virtual-output request
/// ignores it). Best effort: without it the output simply stays at 1x.
fn set_scale(output: &str, scale: f64) {
    let run = |args: &[&str]| {
        std::process::Command::new("kscreen-doctor")
            .args(args)
            .output()
    };
    let Ok(out) = run(&["-j"]) else {
        warn!("kscreen-doctor not found; set the virtual monitor's scale in Display Settings");
        return;
    };
    let id = serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| {
            v["outputs"]
                .as_array()?
                .iter()
                .find(|o| o["name"] == output)?["id"]
                .as_u64()
        });
    let Some(id) = id else {
        warn!("could not find output \"{output}\" to set its scale");
        return;
    };
    let arg = format!("output.{id}.scale.{}", (scale * 100.0).round() / 100.0);
    match run(&[&arg]) {
        Ok(o) if o.status.success() => info!("set virtual monitor scale to {scale}"),
        _ => warn!("kscreen-doctor {arg} failed"),
    }
}

/// Installs (or updates) `~/.local/share/applications/silkscreen.desktop`
/// so KWin grants this executable the interfaces above. Returns `true` if
/// the file changed — KWin reads it when a client connects, so the change
/// applies to the next connection.
pub fn ensure_desktop_entry() -> Result<bool> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = directories::BaseDirs::new()
        .context("no home directory")?
        .data_dir()
        .join("applications");
    let path = dir.join("silkscreen.desktop");
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Silkscreen\n\
         Comment=Use another computer as a wireless display\n\
         Exec={} display\n\
         Icon=video-display\n\
         Categories=Utility;\n\
         X-KDE-Wayland-Interfaces={SCREENCAST},{FAKE_INPUT}\n",
        exe.display()
    );
    if std::fs::read_to_string(&path).is_ok_and(|c| c == contents) {
        return Ok(false);
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, contents)?;
    info!(
        "installed {} so KWin allows virtual monitors",
        path.display()
    );
    // Refresh KDE's service cache so KWin sees the entry immediately.
    let _ = std::process::Command::new("kbuildsycoca6")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(true)
}
