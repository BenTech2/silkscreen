//! The stream's frame rate limit: lets at most one frame through per
//! interval, so a slow link (Wi-Fi) gets fewer, not later, frames. A frame
//! that arrives too soon is kept (copied) and sent when its turn comes, so
//! the last change on screen always reaches the display.

use std::time::{Duration, Instant};

use silkscreen_platform::{Frame, PixelLayout};

pub struct Pacer {
    interval: Option<Duration>,
    /// When the next frame may go. Advanced by whole intervals, so a 30 fps
    /// limit on a 75 Hz screen averages 30 (gaps of 2 and 3 frames) rather
    /// than settling on every third frame (25).
    next: Option<Instant>,
    held: Vec<u8>,
    held_frame: Option<(u32, u32, usize, PixelLayout)>,
}

impl Pacer {
    pub fn new(max_fps: Option<f32>) -> Self {
        let mut p = Pacer {
            interval: None,
            next: None,
            held: Vec::new(),
            held_frame: None,
        };
        p.set_max_fps(max_fps);
        p
    }

    pub fn set_max_fps(&mut self, max_fps: Option<f32>) {
        self.interval = max_fps
            .filter(|f| *f > 0.0)
            .map(|f| Duration::from_secs_f32(1.0 / f));
        if self.interval.is_none() {
            self.held_frame = None;
            self.next = None;
        }
    }

    fn due(&self) -> bool {
        // A little early is fine: capture times jitter around the rate.
        self.next
            .is_none_or(|t| Instant::now() + Duration::from_millis(2) >= t)
    }

    fn sent(&mut self) {
        if let Some(iv) = self.interval {
            let now = Instant::now();
            // After a quiet spell, start counting from now (no burst).
            let next = self.next.map_or(now, |t| t.max(now - iv)) + iv;
            self.next = Some(next);
        }
    }

    /// For a newly captured frame: true to encode it now (`force`: a
    /// keyframe is wanted), false if it was held back for later.
    pub fn admit(&mut self, frame: &Frame, force: bool) -> bool {
        if force || self.due() {
            self.sent();
            self.held_frame = None;
            return true;
        }
        self.held.clear();
        self.held.extend_from_slice(frame.data);
        self.held_frame = Some((frame.width, frame.height, frame.stride, frame.layout));
        false
    }

    /// A held-back frame whose turn has come.
    pub fn take_due(&mut self) -> Option<Frame<'_>> {
        if self.held_frame.is_none() || !self.due() {
            return None;
        }
        let (width, height, stride, layout) = self.held_frame.take()?;
        self.sent();
        Some(Frame {
            width,
            height,
            stride,
            layout,
            data: &self.held,
        })
    }
}
