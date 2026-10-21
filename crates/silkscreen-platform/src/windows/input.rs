//! Input injection on Windows.
//!
//! Touch and pen use synthetic pointer devices (`InjectSyntheticPointerInput`,
//! Windows 10 1809+), so apps see genuine multitouch and pen input with
//! pressure and tilt — not a mouse pretending. Mouse uses `SendInput`.

use std::collections::BTreeMap;

use anyhow::Result;
use silkscreen_proto::{InputEvent, MouseButton, TouchPhase};
use tracing::{debug, warn};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_PAUSE,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
    POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_FLAGS, POINTER_INFO,
    POINTER_PEN_INFO, POINTER_TOUCH_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PT_PEN, PT_TOUCH, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

use crate::InputSink;

const MAX_CONTACTS: u32 = 10;
// Values from the Windows SDK (winuser.h).
const TOUCH_MASK_CONTACTAREA: u32 = 0x1;
const TOUCH_MASK_PRESSURE: u32 = 0x4;
const PEN_FLAG_BARREL: u32 = 0x1;
const PEN_FLAG_INVERTED: u32 = 0x2;
const PEN_FLAG_ERASER: u32 = 0x4;
const PEN_MASK_PRESSURE: u32 = 0x1;
const PEN_MASK_TILT_X: u32 = 0x4;
const PEN_MASK_TILT_Y: u32 = 0x8;
const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

pub struct WindowsInput {
    /// GDI device name of the target monitor, e.g. `\\.\DISPLAY3`.
    device: Vec<u16>,
    touch: Option<HSYNTHETICPOINTERDEVICE>,
    pen: Option<HSYNTHETICPOINTERDEVICE>,
    /// Active touch contacts: display id -> (slot, x, y, pressure).
    contacts: BTreeMap<u32, (u32, i32, i32, u32)>,
    pen_state: PenState,
}

#[derive(Default, PartialEq)]
enum PenState {
    #[default]
    Away,
    Hover,
    Contact,
}

unsafe impl Send for WindowsInput {}

impl WindowsInput {
    pub fn new(device_name: &str) -> Self {
        let create = |kind, n| match unsafe {
            CreateSyntheticPointerDevice(kind, n, POINTER_FEEDBACK_DEFAULT)
        } {
            Ok(d) => Some(d),
            Err(e) => {
                warn!(
                    "synthetic pointer device unavailable ({e}); falling back to mouse emulation"
                );
                None
            }
        };
        WindowsInput {
            device: device_name.encode_utf16().chain([0]).collect(),
            touch: create(PT_TOUCH, MAX_CONTACTS),
            pen: create(PT_PEN, 1),
            contacts: BTreeMap::new(),
            pen_state: PenState::Away,
        }
    }

    fn to_px(&self, x: f32, y: f32) -> (i32, i32) {
        // Looked up each time: the monitor moves when modes or layout change.
        let r = super::monitor_rect(&self.device).unwrap_or_default();
        let w = (r.right - r.left) as f32;
        let h = (r.bottom - r.top) as f32;
        (
            r.left + (x.clamp(0.0, 1.0) * (w - 1.0)).round() as i32,
            r.top + (y.clamp(0.0, 1.0) * (h - 1.0)).round() as i32,
        )
    }

    fn touch(
        &mut self,
        id: u32,
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: Option<f32>,
    ) -> Result<()> {
        let Some(dev) = self.touch else {
            return self.mouse_emulate(phase, x, y);
        };
        let (px, py) = self.to_px(x, y);
        let p = (pressure.unwrap_or(0.5).clamp(0.0, 1.0) * 1024.0) as u32;
        match phase {
            TouchPhase::Down => {
                if self.contacts.len() >= MAX_CONTACTS as usize {
                    return Ok(());
                }
                let slot = (0..MAX_CONTACTS)
                    .find(|s| !self.contacts.values().any(|c| c.0 == *s))
                    .unwrap();
                self.contacts.insert(id, (slot, px, py, p));
            }
            TouchPhase::Move => {
                if let Some(c) = self.contacts.get_mut(&id) {
                    (c.1, c.2, c.3) = (px, py, p);
                }
            }
            TouchPhase::Up | TouchPhase::Cancel => {}
        }
        // Windows wants every active contact in each injection frame.
        let mut frame = Vec::with_capacity(self.contacts.len());
        for (&cid, &(slot, cx, cy, cp)) in &self.contacts {
            let flags = if cid == id {
                match phase {
                    TouchPhase::Down => {
                        POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
                    }
                    TouchPhase::Move => {
                        POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
                    }
                    TouchPhase::Up => POINTER_FLAG_UP,
                    TouchPhase::Cancel => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
                }
            } else {
                POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
            };
            frame.push(touch_info(slot, cx, cy, cp, flags));
        }
        if matches!(phase, TouchPhase::Up | TouchPhase::Cancel) {
            self.contacts.remove(&id);
        }
        if !frame.is_empty() {
            unsafe { InjectSyntheticPointerInput(dev, &frame)? };
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn pen(
        &mut self,
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: f32,
        tilt: (f32, f32),
        barrel: bool,
        eraser: bool,
        hover: bool,
    ) -> Result<()> {
        let Some(dev) = self.pen else {
            return self.mouse_emulate(if hover { TouchPhase::Move } else { phase }, x, y);
        };
        let (px, py) = self.to_px(x, y);
        let next = match phase {
            TouchPhase::Down | TouchPhase::Move if hover => PenState::Hover,
            TouchPhase::Down | TouchPhase::Move => PenState::Contact,
            TouchPhase::Up => PenState::Hover,
            TouchPhase::Cancel => PenState::Away,
        };
        let flags = match (&self.pen_state, &next) {
            (PenState::Contact, PenState::Contact) => {
                POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
            }
            (_, PenState::Contact) => {
                POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
            }
            (PenState::Contact, PenState::Hover) => POINTER_FLAG_UP | POINTER_FLAG_INRANGE,
            (_, PenState::Hover) => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE,
            (PenState::Contact, PenState::Away) => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
            (_, PenState::Away) => POINTER_FLAG_UPDATE,
        };
        self.pen_state = next;
        let mut pen_flags = 0;
        if barrel {
            pen_flags |= PEN_FLAG_BARREL;
        }
        if eraser {
            pen_flags |= PEN_FLAG_ERASER | PEN_FLAG_INVERTED;
        }
        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: windows::Win32::UI::Controls::POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: pointer_info(PT_PEN, 0, px, py, flags),
                    penFlags: pen_flags,
                    penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
                    pressure: (pressure.clamp(0.0, 1.0) * 1024.0) as u32,
                    rotation: 0,
                    tiltX: tilt.0.clamp(-90.0, 90.0) as i32,
                    tiltY: tilt.1.clamp(-90.0, 90.0) as i32,
                },
            },
        };
        unsafe { InjectSyntheticPointerInput(dev, &[info])? };
        Ok(())
    }

    fn mouse_emulate(&mut self, phase: TouchPhase, x: f32, y: f32) -> Result<()> {
        self.mouse_move(x, y);
        match phase {
            TouchPhase::Down => send_mouse(0, 0, 0, MOUSEEVENTF_LEFTDOWN),
            TouchPhase::Up | TouchPhase::Cancel => send_mouse(0, 0, 0, MOUSEEVENTF_LEFTUP),
            TouchPhase::Move => {}
        }
        Ok(())
    }

    fn mouse_move(&self, x: f32, y: f32) {
        let (px, py) = self.to_px(x, y);
        // Absolute coordinates are 0..65535 across the whole virtual desktop.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
            )
        };
        let nx = ((px - vx) as i64 * 65535 / (vw - 1) as i64) as i32;
        let ny = ((py - vy) as i64 * 65535 / (vh - 1) as i64) as i32;
        send_mouse(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        );
    }
}

impl Drop for WindowsInput {
    fn drop(&mut self) {
        for dev in [self.touch.take(), self.pen.take()].into_iter().flatten() {
            unsafe { DestroySyntheticPointerDevice(dev) };
        }
    }
}

impl InputSink for WindowsInput {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        match *ev {
            InputEvent::Touch {
                id,
                phase,
                x,
                y,
                pressure,
            } => self.touch(id, phase, x, y, pressure)?,
            InputEvent::Pen {
                phase,
                x,
                y,
                pressure,
                tilt_x,
                tilt_y,
                barrel,
                eraser,
                hover,
            } => self.pen(
                phase,
                x,
                y,
                pressure,
                (tilt_x, tilt_y),
                barrel,
                eraser,
                hover,
            )?,
            InputEvent::MouseMove { x, y } => self.mouse_move(x, y),
            InputEvent::MouseButton { button, pressed } => {
                let (flags, data) = match (button, pressed) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                    (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                    (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                    (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
                };
                send_mouse(0, 0, data, flags);
            }
            InputEvent::Scroll { dx, dy, pixels } => {
                // WHEEL_DELTA (120) per notch; pixel deltas are roughly 1/4 notch per 30px.
                let k = if pixels { 4.0 } else { 120.0 };
                if dy != 0.0 {
                    send_mouse(0, 0, (dy * k) as i32 as u32, MOUSEEVENTF_WHEEL);
                }
                if dx != 0.0 {
                    send_mouse(0, 0, (-dx * k) as i32 as u32, MOUSEEVENTF_HWHEEL);
                }
            }
            InputEvent::Key {
                evdev_code,
                pressed,
            } => send_key(evdev_code, pressed),
        }
        Ok(())
    }
}

fn pointer_info(
    kind: windows::Win32::UI::WindowsAndMessaging::POINTER_INPUT_TYPE,
    id: u32,
    x: i32,
    y: i32,
    flags: POINTER_FLAGS,
) -> POINTER_INFO {
    POINTER_INFO {
        pointerType: kind,
        pointerId: id,
        pointerFlags: flags,
        ptPixelLocation: POINT { x, y },
        ..Default::default()
    }
}

fn touch_info(slot: u32, x: i32, y: i32, pressure: u32, flags: POINTER_FLAGS) -> POINTER_TYPE_INFO {
    POINTER_TYPE_INFO {
        r#type: PT_TOUCH,
        Anonymous: windows::Win32::UI::Controls::POINTER_TYPE_INFO_0 {
            touchInfo: POINTER_TOUCH_INFO {
                pointerInfo: pointer_info(PT_TOUCH, slot, x, y, flags),
                touchMask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_PRESSURE,
                rcContact: RECT {
                    left: x - 2,
                    top: y - 2,
                    right: x + 2,
                    bottom: y + 2,
                },
                pressure,
                ..Default::default()
            },
        },
    }
}

/// Sends a key by scancode, so it means the same physical key regardless of
/// the host's keyboard layout.
fn send_key(evdev: u16, pressed: bool) {
    let up = if pressed {
        KEYBD_EVENT_FLAGS(0)
    } else {
        KEYEVENTF_KEYUP
    };
    let ki = match crate::keymap::windows_scancode(evdev) {
        Some(sc) => {
            let mut flags = KEYEVENTF_SCANCODE | up;
            if sc & 0xE000 == 0xE000 {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: sc & 0xFF,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            }
        }
        None if evdev == 119 => KEYBDINPUT {
            wVk: VK_PAUSE,
            wScan: 0,
            dwFlags: up,
            time: 0,
            dwExtraInfo: 0,
        },
        None => {
            debug!("no Windows key for evdev {evdev}");
            return;
        }
    };
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    };
    unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
}

fn send_mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
}
