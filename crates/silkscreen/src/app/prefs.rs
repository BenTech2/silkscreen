//! The app's own settings (display and stream options), saved in
//! `gui.json` next to `settings.json` whenever they change in the window.
//! Command-line flags override them for one run.

use serde::{Deserialize, Serialize};
use silkscreen_proto::{Codec, DEFAULT_CONTROL_PORT, DEFAULT_DATAGRAM_LEN};

use crate::host::Resolution;
use crate::secure::config_dir;

const FILE: &str = "gui.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceChoice {
    Virtual,
    Monitor,
    TestPattern,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub display: DisplayPrefs,
    pub stream: StreamPrefs,
    pub app: AppPrefs,
}

/// What the app does when it starts at login.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginStart {
    /// Open the window as usual.
    #[default]
    Window,
    /// Stay in the system tray (needs the tray icon).
    Tray,
    /// Go straight into display mode.
    Display,
}

/// The app itself: tray icon and starting at login (both off by default).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppPrefs {
    /// Show a tray icon; closing the window then keeps Silkscreen running.
    pub tray: bool,
    pub start_at_login: bool,
    pub login_start: LoginStart,
}

/// Display mode (`silkscreen display`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayPrefs {
    pub windowed: bool,
    /// Index into the system's monitor list; `None` = the one the window is on.
    pub monitor: Option<usize>,
    pub port: u16,
    pub hw_decode: bool,
    pub vsync: bool,
    pub forward_mouse: bool,
    pub forward_keyboard: bool,
    /// Skip queued frames when the picture falls this many ms behind the
    /// host; `None` = show every frame.
    pub skip_behind_ms: Option<u32>,
}

impl Default for DisplayPrefs {
    fn default() -> Self {
        DisplayPrefs {
            windowed: false,
            monitor: None,
            port: DEFAULT_CONTROL_PORT,
            hw_decode: true,
            vsync: true,
            forward_mouse: true,
            forward_keyboard: true,
            skip_behind_ms: None,
        }
    }
}

/// Extending this computer to a display (`silkscreen host`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamPrefs {
    pub source: SourceChoice,
    pub monitor_name: String,
    /// The screen Mirror sends (an id from `silkscreen_platform::monitors`);
    /// `None` = the main screen.
    pub mirror_screen: Option<String>,
    pub codec: Option<Codec>,
    /// A specific FFmpeg encoder for `codec`; `None` tries the usual ones.
    pub encoder: Option<String>,
    /// 0 = automatic.
    pub bitrate_mbps: f32,
    pub resolution: Resolution,
    /// Overrides `resolution`.
    pub size: Option<(u32, u32)>,
    pub refresh_hz: Option<f32>,
    /// Send at most this many frames a second; `None` = every frame.
    pub max_fps: Option<u32>,
    pub datagram_len: usize,
    /// Reconnect when the display goes away (`--once` turns this off).
    pub reconnect: bool,
}

impl Default for StreamPrefs {
    fn default() -> Self {
        StreamPrefs {
            source: SourceChoice::Virtual,
            monitor_name: String::new(),
            mirror_screen: None,
            codec: None,
            encoder: None,
            bitrate_mbps: 0.0,
            resolution: Resolution::Auto,
            size: None,
            refresh_hz: None,
            max_fps: None,
            datagram_len: DEFAULT_DATAGRAM_LEN,
            reconnect: true,
        }
    }
}

impl Prefs {
    pub fn load() -> Self {
        std::fs::read(config_dir().join(FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let r = std::fs::create_dir_all(config_dir()).and_then(|_| {
            std::fs::write(
                config_dir().join(FILE),
                serde_json::to_vec_pretty(self).unwrap(),
            )
        });
        if let Err(e) = r {
            tracing::warn!("could not save settings: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut p = Prefs::default();
        p.display.monitor = Some(1);
        p.stream.codec = Some(Codec::Hevc);
        p.stream.size = Some((2560, 1440));
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Prefs>(&json).unwrap(), p);
    }

    #[test]
    fn missing_fields_keep_defaults() {
        let p: Prefs = serde_json::from_str(r#"{"display": {"vsync": false}}"#).unwrap();
        assert!(!p.display.vsync);
        assert!(p.display.hw_decode && p.stream.reconnect);
        assert_eq!(p.display.port, DEFAULT_CONTROL_PORT);
    }
}
