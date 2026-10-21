//! Pointer injection with CGEvent (needs Accessibility permission). Touch
//! and pen become a left-button drag, the way a single-touch screen behaves.

use anyhow::Result;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton,
    CGScrollEventUnit,
};
use silkscreen_proto::{InputEvent, MouseButton, TouchPhase};

use crate::InputSink;

pub struct MacInput {
    display: u32,
    /// Which button is held, so moves become drags.
    held: Option<MouseButton>,
    /// Modifier flags implied by modifier keys currently held.
    flags: CGEventFlags,
    /// The touch id currently driving the pointer.
    touch_id: Option<u32>,
}

impl MacInput {
    pub fn new(display: u32) -> Self {
        MacInput {
            display,
            held: None,
            flags: CGEventFlags(0),
            touch_id: None,
        }
    }

    fn point(&self, x: f32, y: f32) -> CGPoint {
        let b = CGDisplayBounds(self.display);
        CGPoint {
            x: b.origin.x + x.clamp(0.0, 1.0) as f64 * b.size.width,
            y: b.origin.y + y.clamp(0.0, 1.0) as f64 * b.size.height,
        }
    }

    fn post(kind: CGEventType, at: CGPoint, button: CGMouseButton) {
        if let Some(ev) = CGEvent::new_mouse_event(None, kind, at, button) {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
        }
    }

    fn move_to(&self, at: CGPoint) {
        let (kind, button) = match self.held {
            Some(MouseButton::Left) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
            Some(MouseButton::Right) => (CGEventType::RightMouseDragged, CGMouseButton::Right),
            Some(_) => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
            None => (CGEventType::MouseMoved, CGMouseButton::Left),
        };
        Self::post(kind, at, button);
    }

    fn button(&mut self, button: MouseButton, pressed: bool, at: CGPoint) {
        let (down, up, b) = match button {
            MouseButton::Left => (
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
                CGMouseButton::Left,
            ),
            MouseButton::Right => (
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
                CGMouseButton::Right,
            ),
            _ => (
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                CGMouseButton::Center,
            ),
        };
        Self::post(if pressed { down } else { up }, at, b);
        self.held = pressed.then_some(button);
    }

    fn key(&mut self, evdev: u16, pressed: bool) {
        let Some(vk) = crate::keymap::mac_keycode(evdev) else {
            return;
        };
        let modifier = match evdev {
            42 | 54 => CGEventFlags::MaskShift,
            29 | 97 => CGEventFlags::MaskControl,
            56 | 100 => CGEventFlags::MaskAlternate,
            125 | 126 => CGEventFlags::MaskCommand,
            _ => CGEventFlags(0),
        };
        if pressed {
            self.flags.0 |= modifier.0;
        } else {
            self.flags.0 &= !modifier.0;
        }
        if let Some(ev) = CGEvent::new_keyboard_event(None, vk, pressed) {
            // Posted events don't inherit modifier state; carry it explicitly.
            CGEvent::set_flags(Some(&ev), self.flags);
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
        }
    }

    fn current(&self) -> CGPoint {
        CGEvent::new(None)
            .map(|e| CGEvent::location(Some(&e)))
            .unwrap_or(CGPoint { x: 0.0, y: 0.0 })
    }
}

impl InputSink for MacInput {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        match *ev {
            InputEvent::Touch {
                id, phase, x, y, ..
            } => {
                // Only the first finger drives the pointer.
                if self.touch_id.is_some_and(|t| t != id) {
                    return Ok(());
                }
                let at = self.point(x, y);
                match phase {
                    TouchPhase::Down => {
                        self.touch_id = Some(id);
                        Self::post(CGEventType::MouseMoved, at, CGMouseButton::Left);
                        self.button(MouseButton::Left, true, at);
                    }
                    TouchPhase::Move => self.move_to(at),
                    TouchPhase::Up | TouchPhase::Cancel => {
                        self.touch_id = None;
                        self.button(MouseButton::Left, false, at);
                    }
                }
            }
            InputEvent::Pen {
                phase, x, y, hover, ..
            } => {
                let at = self.point(x, y);
                let contact = matches!(phase, TouchPhase::Down | TouchPhase::Move) && !hover;
                let held = self.held == Some(MouseButton::Left);
                if contact != held {
                    self.button(MouseButton::Left, contact, at);
                } else {
                    self.move_to(at);
                }
            }
            InputEvent::MouseMove { x, y } => self.move_to(self.point(x, y)),
            InputEvent::MouseButton { button, pressed } => {
                self.button(button, pressed, self.current())
            }
            InputEvent::Scroll { dx, dy, pixels } => {
                let (unit, k) = if pixels {
                    (CGScrollEventUnit::Pixel, 1.0)
                } else {
                    (CGScrollEventUnit::Line, 1.0)
                };
                if let Some(e) = CGEvent::new_scroll_wheel_event2(
                    None,
                    unit,
                    2,
                    (dy * k) as i32,
                    (dx * k) as i32,
                    0,
                ) {
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&e));
                }
            }
            InputEvent::Key {
                evdev_code,
                pressed,
            } => self.key(evdev_code, pressed),
        }
        Ok(())
    }
}
