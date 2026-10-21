//! xdg-desktop-portal backend (KDE, GNOME, and any compositor with a
//! ScreenCast + RemoteDesktop portal).
//!
//! One combined RemoteDesktop/ScreenCast session provides both the video
//! stream (a *virtual* monitor or an existing one) and input injection —
//! including real multitouch — targeted at that stream. The permission grant
//! is persisted with a restore token so the dialog is only shown once.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::PersistMode;
use ashpd::desktop::remote_desktop::{
    Axis, DeviceType, KeyState, NotifyPointerAxisOptions, RemoteDesktop, SelectDevicesOptions,
};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use silkscreen_proto::{InputEvent, MouseButton, TouchPhase};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::InputSink;

pub struct PortalStream {
    pub fd: OwnedFd,
    pub node_id: u32,
    pub size: Option<(i32, i32)>,
    /// Dropping this closes the portal session (removing a virtual monitor).
    pub guard: Box<dyn Send>,
    pub input: Box<dyn InputSink>,
    pub touch: bool,
}

struct Started {
    fd: OwnedFd,
    node_id: u32,
    size: Option<(i32, i32)>,
    touch: bool,
}

/// Opens a portal session. `virtual_monitor` selects a new virtual output;
/// otherwise the user picks an existing monitor.
pub fn open(virtual_monitor: bool, want_touch: bool) -> Result<PortalStream> {
    let (started_tx, started_rx) = std::sync::mpsc::channel::<Result<Started>>();
    let (input_tx, input_rx) = mpsc::unbounded_channel::<InputEvent>();
    let (close_tx, close_rx) = oneshot::channel::<()>();

    std::thread::Builder::new()
        .name("portal".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = started_tx.send(Err(e.into()));
                    return;
                }
            };
            rt.block_on(session(
                virtual_monitor,
                want_touch,
                started_tx,
                input_rx,
                close_rx,
            ));
        })?;

    info!("waiting for screen-sharing permission (a system dialog may appear)…");
    let started = started_rx
        .recv_timeout(Duration::from_secs(120))
        .map_err(|_| anyhow!("timed out waiting for the screen-sharing portal"))??;
    Ok(PortalStream {
        fd: started.fd,
        node_id: started.node_id,
        size: started.size,
        guard: Box::new(close_tx),
        input: Box::new(PortalInput { tx: input_tx }),
        touch: started.touch,
    })
}

struct PortalInput {
    tx: mpsc::UnboundedSender<InputEvent>,
}

impl InputSink for PortalInput {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        self.tx
            .send(*ev)
            .map_err(|_| anyhow!("portal session closed"))
    }
}

fn token_path(virtual_monitor: bool) -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "silkscreen")?;
    Some(dirs.config_dir().join(if virtual_monitor {
        "portal-virtual.token"
    } else {
        "portal-monitor.token"
    }))
}

async fn session(
    virtual_monitor: bool,
    want_touch: bool,
    started: std::sync::mpsc::Sender<Result<Started>>,
    mut input: mpsc::UnboundedReceiver<InputEvent>,
    mut close: oneshot::Receiver<()>,
) {
    let setup = async {
        let rd = RemoteDesktop::new()
            .await
            .context("RemoteDesktop portal unavailable")?;
        let sc = Screencast::new()
            .await
            .context("ScreenCast portal unavailable")?;
        let session = rd.create_session(Default::default()).await?;

        let available = rd.available_device_types().await.unwrap_or_default();
        let mut devices = DeviceType::Pointer | DeviceType::Keyboard;
        let touch = want_touch && available.contains(DeviceType::Touchscreen);
        if touch {
            devices |= DeviceType::Touchscreen;
        }
        let token_file = token_path(virtual_monitor);
        let token = token_file
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok());

        rd.select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(devices)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref()),
        )
        .await?;
        sc.select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(ashpd::enumflags2::BitFlags::from(if virtual_monitor {
                    SourceType::Virtual
                } else {
                    SourceType::Monitor
                }))
                .set_multiple(false),
        )
        .await?;
        let response = rd
            .start(&session, None, Default::default())
            .await?
            .response()?;
        let stream = response
            .streams()
            .first()
            .cloned()
            .context("portal returned no stream")?;

        if let (Some(path), Some(token)) = (&token_file, response.restore_token()) {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let _ = std::fs::write(path, token);
        }
        let touch = touch && response.devices().contains(DeviceType::Touchscreen);
        let fd = sc
            .open_pipe_wire_remote(&session, Default::default())
            .await?;
        info!(
            "portal stream node {} size {:?} (input: {:?})",
            stream.pipe_wire_node_id(),
            stream.size(),
            response.devices()
        );
        anyhow::Ok((rd, session, stream, fd, touch))
    };

    let (rd, session, stream, fd, touch) = match setup.await {
        Ok(v) => v,
        Err(e) => {
            let _ = started.send(Err(e));
            return;
        }
    };
    let node = stream.pipe_wire_node_id();
    let (w, h) = stream
        .size()
        .map(|(w, h)| (w as f64, h as f64))
        .unwrap_or((1920.0, 1080.0));
    let _ = started.send(Ok(Started {
        fd,
        node_id: node,
        size: stream.size(),
        touch,
    }));

    // Touch ids from the display can be arbitrary; portals want small slots.
    let mut slots: HashMap<u32, u32> = HashMap::new();
    loop {
        let ev = tokio::select! {
            ev = input.recv() => match ev { Some(ev) => ev, None => break },
            _ = &mut close => break,
        };
        let r = match ev {
            InputEvent::Touch {
                id, phase, x, y, ..
            } if touch => {
                let (x, y) = (x as f64 * w, y as f64 * h);
                match phase {
                    TouchPhase::Down => {
                        let slot = (0..).find(|s| !slots.values().any(|v| v == s)).unwrap();
                        slots.insert(id, slot);
                        rd.notify_touch_down(&session, node, slot, x, y, Default::default())
                            .await
                    }
                    TouchPhase::Move => match slots.get(&id) {
                        Some(&slot) => {
                            rd.notify_touch_motion(&session, node, slot, x, y, Default::default())
                                .await
                        }
                        None => Ok(()),
                    },
                    TouchPhase::Up | TouchPhase::Cancel => match slots.remove(&id) {
                        Some(slot) => rd.notify_touch_up(&session, slot, Default::default()).await,
                        None => Ok(()),
                    },
                }
            }
            // Without touch permission, a single touch drives the pointer.
            InputEvent::Touch { phase, x, y, .. } => {
                let r = rd
                    .notify_pointer_motion_absolute(
                        &session,
                        node,
                        x as f64 * w,
                        y as f64 * h,
                        Default::default(),
                    )
                    .await;
                match phase {
                    TouchPhase::Down => {
                        rd.notify_pointer_button(
                            &session,
                            BTN_LEFT,
                            KeyState::Pressed,
                            Default::default(),
                        )
                        .await
                    }
                    TouchPhase::Up | TouchPhase::Cancel => {
                        rd.notify_pointer_button(
                            &session,
                            BTN_LEFT,
                            KeyState::Released,
                            Default::default(),
                        )
                        .await
                    }
                    TouchPhase::Move => r,
                }
            }
            InputEvent::Pen {
                phase, x, y, hover, ..
            } => {
                let r = rd
                    .notify_pointer_motion_absolute(
                        &session,
                        node,
                        x as f64 * w,
                        y as f64 * h,
                        Default::default(),
                    )
                    .await;
                match phase {
                    TouchPhase::Down if !hover => {
                        rd.notify_pointer_button(
                            &session,
                            BTN_LEFT,
                            KeyState::Pressed,
                            Default::default(),
                        )
                        .await
                    }
                    TouchPhase::Up | TouchPhase::Cancel => {
                        rd.notify_pointer_button(
                            &session,
                            BTN_LEFT,
                            KeyState::Released,
                            Default::default(),
                        )
                        .await
                    }
                    _ => r,
                }
            }
            InputEvent::MouseMove { x, y } => {
                rd.notify_pointer_motion_absolute(
                    &session,
                    node,
                    x as f64 * w,
                    y as f64 * h,
                    Default::default(),
                )
                .await
            }
            InputEvent::MouseButton { button, pressed } => {
                let state = if pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                rd.notify_pointer_button(&session, evdev_button(button), state, Default::default())
                    .await
            }
            InputEvent::Scroll {
                dx,
                dy,
                pixels: false,
            } => {
                // Display: positive = up/left. evdev: positive = down/right.
                let mut r = Ok(());
                if dy != 0.0 {
                    r = rd
                        .notify_pointer_axis_discrete(
                            &session,
                            Axis::Vertical,
                            -dy.round() as i32,
                            Default::default(),
                        )
                        .await;
                }
                if dx != 0.0 {
                    r = rd
                        .notify_pointer_axis_discrete(
                            &session,
                            Axis::Horizontal,
                            -dx.round() as i32,
                            Default::default(),
                        )
                        .await;
                }
                r
            }
            InputEvent::Scroll {
                dx,
                dy,
                pixels: true,
            } => {
                rd.notify_pointer_axis(
                    &session,
                    -dx as f64,
                    -dy as f64,
                    NotifyPointerAxisOptions::default().set_finish(true),
                )
                .await
            }
            InputEvent::Key {
                evdev_code,
                pressed,
            } => {
                let state = if pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                rd.notify_keyboard_keycode(&session, evdev_code as i32, state, Default::default())
                    .await
            }
        };
        if let Err(e) = r {
            debug!("portal input: {e}");
        }
    }
    if let Err(e) = session.close().await {
        warn!("closing portal session: {e}");
    }
}

const BTN_LEFT: i32 = 0x110;

fn evdev_button(b: MouseButton) -> i32 {
    match b {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Back => 0x113,
        MouseButton::Forward => 0x114,
    }
}
