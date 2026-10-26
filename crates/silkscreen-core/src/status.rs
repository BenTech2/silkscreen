//! Live session state that the GUI (and debug overlay) displays.

use std::sync::{Arc, Mutex};

use silkscreen_proto::ReceiverStats;

use crate::ctx::wake_ui;

/// A `Mutex` that wakes the GUI whenever it is modified through [`update`](Shared::update).
pub struct Shared<T>(Mutex<T>);

impl<T: Clone + Default> Shared<T> {
    pub fn new() -> Arc<Self> {
        Arc::new(Shared(Mutex::new(T::default())))
    }

    pub fn get(&self) -> T {
        self.0.lock().unwrap().clone()
    }

    pub fn update(&self, f: impl FnOnce(&mut T)) {
        f(&mut self.0.lock().unwrap());
        wake_ui();
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum HostPhase {
    #[default]
    Idle,
    Connecting(String),
    /// Waiting for the user to type the code shown on this display.
    NeedCode(String),
    Streaming,
    /// Lost the display; retrying.
    Reconnecting(String),
    /// Gave up (refused, wrong code, cancelled) or stopped by the user.
    Stopped(Option<String>),
}

#[derive(Debug, Clone, Default)]
pub struct HostState {
    pub phase: HostPhase,
    pub display: Option<String>,
    /// e.g. "KWin virtual output … (2560x1600)".
    pub source: Option<String>,
    /// e.g. "4500x3000 @ 60 Hz · hevc_nvenc · 56.7 Mbit/s".
    pub stream: Option<String>,
    pub fps: f32,
    pub mbps: f32,
    pub encode_ms: f32,
    pub rtt_ms: f32,
    pub keyframes: u32,
    pub receiver: ReceiverStats,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum DisplayPhase {
    #[default]
    Off,
    Waiting,
    /// Show this code so the host's user can type it.
    Pairing {
        host: String,
        code: String,
    },
    Streaming {
        host: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct DisplayState {
    pub phase: DisplayPhase,
    pub stream: Option<String>,
    pub decoder: Option<String>,
    pub fps: f32,
    /// First packet received → frame decoded and uploaded.
    pub latency_ms: f32,
    pub frames_dropped: u32,
    pub packets_lost: u32,
    /// How far behind the host the picture is (worst in the last second).
    pub behind_ms: f32,
    /// Times this display skipped ahead to catch up.
    pub catch_ups: u32,
    /// Something to tell the user while waiting, e.g. that a host tried to
    /// connect with an incompatible version. Cleared when a host gets in.
    pub notice: Option<String>,
}
