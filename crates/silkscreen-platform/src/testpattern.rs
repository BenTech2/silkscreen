//! Synthetic source: moving colour bars plus a circle under every active
//! touch/pen/mouse contact, so the whole pipeline — including input — can be
//! verified without screen-capture permissions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use rayon::prelude::*;
use silkscreen_proto::{InputEvent, TouchPhase};

use crate::{Capturer, Frame, InputSink, PixelLayout, Screen, ScreenRequest};

#[derive(Default)]
struct Contacts {
    /// id -> normalised position. Pen uses id u32::MAX, mouse u32::MAX - 1.
    points: HashMap<u32, (f32, f32, bool)>,
    events: u64,
}

pub fn open(req: &ScreenRequest) -> Screen {
    let contacts = Arc::new(Mutex::new(Contacts::default()));
    let (w, h) = (req.width & !1, req.height & !1);
    Screen {
        capturer: Box::new(TestPattern {
            width: w,
            height: h,
            interval: Duration::from_micros(1_000_000_000 / req.refresh_mhz.max(1000) as u64),
            next: Instant::now(),
            frame: 0,
            buf: vec![0; (w * h * 4) as usize],
            contacts: contacts.clone(),
        }),
        input: Box::new(TestInput { contacts }),
        description: format!("test pattern {w}x{h}"),
    }
}

struct TestPattern {
    width: u32,
    height: u32,
    interval: Duration,
    next: Instant,
    frame: u64,
    buf: Vec<u8>,
    contacts: Arc<Mutex<Contacts>>,
}

impl Capturer for TestPattern {
    fn next_frame(&mut self, timeout: Duration, f: &mut dyn FnMut(Frame<'_>)) -> Result<bool> {
        let now = Instant::now();
        if self.next > now {
            let wait = self.next - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(false);
            }
            std::thread::sleep(wait);
        }
        self.next = (self.next + self.interval).max(Instant::now());
        self.draw();
        f(Frame {
            width: self.width,
            height: self.height,
            stride: self.width as usize * 4,
            layout: PixelLayout::Bgrx,
            data: &self.buf,
        });
        self.frame += 1;
        Ok(true)
    }
}

impl TestPattern {
    fn draw(&mut self) {
        let (w, h) = (self.width as usize, self.height as usize);
        const BARS: [[u8; 3]; 8] = [
            [235, 235, 235],
            [235, 235, 16],
            [16, 235, 235],
            [16, 235, 16],
            [235, 16, 235],
            [235, 16, 16],
            [16, 16, 235],
            [16, 16, 16],
        ];
        let shift = (self.frame as usize * 4) % w.max(1);
        let sweep_y = (self.frame as usize * 6) % h.max(1);
        self.buf
            .par_chunks_mut(w * 4)
            .enumerate()
            .for_each(|(y, row)| {
                for x in 0..w {
                    let [r, g, b] = if y.abs_diff(sweep_y) < 4 {
                        [255, 255, 255]
                    } else if y < h * 3 / 4 {
                        BARS[((x + shift) % w) * 8 / w]
                    } else {
                        // Static greyscale ramp at the bottom to judge banding.
                        let v = (x * 255 / w.max(1)) as u8;
                        [v, v, v]
                    };
                    row[x * 4..x * 4 + 4].copy_from_slice(&[b, g, r, 255]);
                }
            });

        let c = self.contacts.lock().unwrap();
        let radius = (w.min(h) / 30).max(8) as i64;
        for &(nx, ny, pressed) in c.points.values() {
            let (cx, cy) = ((nx * w as f32) as i64, (ny * h as f32) as i64);
            let colour = if pressed { [0, 0, 255] } else { [0, 200, 0] };
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    let (x, y) = (cx + dx, cy + dy);
                    if dx * dx + dy * dy <= radius * radius
                        && x >= 0
                        && y >= 0
                        && (x as usize) < w
                        && (y as usize) < h
                    {
                        let p = (y as usize * w + x as usize) * 4;
                        self.buf[p..p + 3].copy_from_slice(&colour);
                    }
                }
            }
        }
    }
}

struct TestInput {
    contacts: Arc<Mutex<Contacts>>,
}

impl InputSink for TestInput {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        let mut c = self.contacts.lock().unwrap();
        c.events += 1;
        match *ev {
            InputEvent::Touch {
                id, phase, x, y, ..
            } => match phase {
                TouchPhase::Down | TouchPhase::Move => {
                    c.points.insert(id, (x, y, true));
                }
                TouchPhase::Up | TouchPhase::Cancel => {
                    c.points.remove(&id);
                }
            },
            InputEvent::Pen {
                phase, x, y, hover, ..
            } => match phase {
                TouchPhase::Down | TouchPhase::Move => {
                    c.points.insert(u32::MAX, (x, y, !hover));
                }
                TouchPhase::Up | TouchPhase::Cancel => {
                    c.points.remove(&u32::MAX);
                }
            },
            InputEvent::MouseMove { x, y } => {
                let pressed = c.points.get(&(u32::MAX - 1)).is_some_and(|p| p.2);
                c.points.insert(u32::MAX - 1, (x, y, pressed));
            }
            InputEvent::MouseButton { pressed, .. } => {
                if let Some(p) = c.points.get_mut(&(u32::MAX - 1)) {
                    p.2 = pressed;
                }
            }
            InputEvent::Key {
                evdev_code,
                pressed,
            } => {
                tracing::info!(
                    "test pattern: key {evdev_code} {}",
                    if pressed { "down" } else { "up" }
                );
            }
            _ => {}
        }
        Ok(())
    }
}
