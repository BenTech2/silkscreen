//! Platform layer: creating a virtual monitor, capturing it, and injecting
//! the display's touch/pen/mouse input back into the host OS.
//!
//! | OS      | Virtual monitor                         | Capture                     | Input                         |
//! |---------|-----------------------------------------|-----------------------------|-------------------------------|
//! | Linux   | xdg-desktop-portal ScreenCast "virtual" | PipeWire                    | RemoteDesktop portal (touch)  |
//! | Windows | Virtual Display Driver (IddCx) / dummy  | DXGI Desktop Duplication    | Synthetic pointer (touch/pen) |
//! | macOS   | CGVirtualDisplay                        | ScreenCaptureKit            | CGEvent (mouse)               |

use std::time::Duration;

use anyhow::Result;
use silkscreen_proto::InputEvent;

pub mod autostart;
mod keymap;
pub mod power;
pub mod testpattern;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// Byte order of a captured 32-bit pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelLayout {
    Bgrx,
    Rgbx,
}

/// A captured frame in CPU memory, valid for the duration of the callback.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub layout: PixelLayout,
    pub data: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// Create a new virtual monitor matching the display (default).
    Virtual,
    /// Mirror/extend onto an existing monitor, e.g. one driven by a dummy
    /// HDMI/DP plug. `None` lets the OS (or user) choose.
    Monitor(Option<String>),
    /// Synthetic animated test pattern; needs no permissions.
    TestPattern,
}

#[derive(Debug, Clone)]
pub struct ScreenRequest {
    /// Name for the virtual monitor, as shown in the OS display settings.
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub scale: f32,
    pub physical_size_mm: Option<(u32, u32)>,
    pub kind: SourceKind,
    /// Whether to set up touch injection.
    pub touch: bool,
}

pub trait Capturer: Send {
    /// Waits up to `timeout` for a new frame. Calls `f` and returns `true` if
    /// one arrived; returns `false` on timeout (e.g. the screen is static).
    fn next_frame(&mut self, timeout: Duration, f: &mut dyn FnMut(Frame<'_>)) -> Result<bool>;
}

pub trait InputSink: Send {
    fn inject(&mut self, ev: &InputEvent) -> Result<()>;
}

/// An open screen source: frames come out of `capturer`, input goes into `input`.
pub struct Screen {
    pub capturer: Box<dyn Capturer>,
    pub input: Box<dyn InputSink>,
    /// Human readable description for logs, e.g. "KDE virtual output 2736x1824".
    pub description: String,
}

/// Opens a screen source. For [`SourceKind::Virtual`] this creates a new
/// monitor that exists until the returned [`Screen`] is dropped — like
/// plugging in a display, and unplugging it again.
pub fn open_screen(req: &ScreenRequest) -> Result<Screen> {
    if req.kind == SourceKind::TestPattern {
        return Ok(testpattern::open(req));
    }
    // Several host sessions may plug in virtual monitors at once. The
    // backends find theirs as "the output that just appeared" (and Windows'
    // driver reads its mode from one registry slot), so one at a time.
    static VIRTUAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time = (req.kind == SourceKind::Virtual)
        .then(|| VIRTUAL.lock().unwrap_or_else(|e| e.into_inner()));
    #[cfg(target_os = "linux")]
    return linux::open_screen(req);
    #[cfg(windows)]
    return windows::open_screen(req);
    #[cfg(target_os = "macos")]
    return macos::open_screen(req);
    #[allow(unreachable_code)]
    Err(anyhow::anyhow!(
        "screen capture is not supported on this platform"
    ))
}

/// Tells the user about a fatal error where there may be no terminal to see
/// it (the Windows app has no console): a message box on Windows.
pub fn alert(title: &str, text: &str) {
    #[cfg(windows)]
    windows::alert(title, text);
    #[cfg(not(windows))]
    let _ = (title, text);
}

/// One of this computer's screens, for picking what to mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    /// What [`SourceKind::Monitor`] takes to capture this screen.
    pub id: String,
    pub description: String,
    pub main: bool,
}

/// This computer's screens. Empty where the system asks the user instead
/// (Linux: the screen-sharing dialog).
pub fn monitors() -> Vec<MonitorInfo> {
    #[cfg(windows)]
    return windows::monitor_list();
    #[cfg(target_os = "macos")]
    return macos::monitor_list();
    #[allow(unreachable_code)]
    Vec::new()
}

/// Describes which backends are available here, for `silkscreen doctor`.
pub fn diagnostics() -> Vec<(String, String)> {
    #[cfg(target_os = "linux")]
    return linux::diagnostics();
    #[cfg(windows)]
    return windows::diagnostics();
    #[cfg(target_os = "macos")]
    return macos::diagnostics();
    #[allow(unreachable_code)]
    Vec::new()
}

/// Registers the app with the desktop where that is how it gets its icon
/// (Linux: a desktop entry plus the icon in the user's hicolor theme).
pub fn install_app_entry() {
    #[cfg(target_os = "linux")]
    if let Err(e) = linux::install_app_entry() {
        tracing::warn!("could not install the desktop entry: {e}");
    }
}

/// Maximum simultaneous touch contacts of this machine's built-in panel, or
/// 0 if it has no touchscreen.
pub fn touch_points() -> u8 {
    #[cfg(target_os = "linux")]
    return linux::touch_points();
    #[cfg(windows)]
    return windows::touch_points();
    #[allow(unreachable_code)]
    0
}

/// Input sink that drops everything, for sources without input support.
pub struct NoInput;

impl InputSink for NoInput {
    fn inject(&mut self, _ev: &InputEvent) -> Result<()> {
        Ok(())
    }
}
