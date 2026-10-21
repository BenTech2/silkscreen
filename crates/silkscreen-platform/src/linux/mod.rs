//! Linux: virtual monitors and capture through xdg-desktop-portal + PipeWire.

mod kwin;
mod portal;
mod pw;

use anyhow::Result;
use tracing::{info, warn};

use crate::{Screen, ScreenRequest, SourceKind};

fn is_kde() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .is_ok_and(|d| d.split(':').any(|p| p.eq_ignore_ascii_case("KDE")))
}

pub fn open_screen(req: &ScreenRequest) -> Result<Screen> {
    let virtual_monitor = req.kind == SourceKind::Virtual;
    if virtual_monitor && is_kde() {
        match open_kwin(req) {
            Ok(s) => return Ok(s),
            Err(e) => warn!("KWin virtual output unavailable ({e:#}); falling back to the portal"),
        }
    }
    if let SourceKind::Monitor(Some(name)) = &req.kind {
        tracing::info!("choose \"{name}\" in the screen-sharing dialog");
    }
    let p = portal::open(virtual_monitor, req.touch)?;
    let size = p
        .size
        .map(|(w, h)| format!("{w}x{h}"))
        .unwrap_or_else(|| "unknown size".into());
    let description = format!(
        "{} via portal ({size}, touch {})",
        if virtual_monitor {
            "virtual monitor"
        } else {
            "monitor"
        },
        if p.touch {
            "on"
        } else {
            "emulated with pointer"
        }
    );
    let capturer = pw::PwCapturer::start(
        pw::CaptureParams {
            remote: pw::Remote::Fd(p.fd),
            node_id: p.node_id,
            width: req.width,
            height: req.height,
            refresh_mhz: req.refresh_mhz,
        },
        p.guard,
    )?;
    Ok(Screen {
        capturer: Box::new(capturer),
        input: p.input,
        description,
    })
}

/// KDE: a virtual output sized exactly to the display, no dialog.
fn open_kwin(req: &ScreenRequest) -> Result<Screen> {
    if kwin::ensure_desktop_entry()? && !kwin::available() {
        info!("KWin needs a moment to pick up the new desktop entry; retrying");
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    let k = kwin::open(req)?;
    let description = format!(
        "KWin virtual output \"{}\" ({}x{} @ {}x)",
        req.name, k.size.0, k.size.1, req.scale
    );
    let capturer = pw::PwCapturer::start(
        pw::CaptureParams {
            remote: pw::Remote::Default,
            node_id: k.node_id,
            width: req.width,
            height: req.height,
            refresh_mhz: req.refresh_mhz,
        },
        k.guard,
    )?;
    Ok(Screen {
        capturer: Box::new(capturer),
        input: k.input,
        description,
    })
}

pub fn diagnostics() -> Vec<(String, String)> {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let mut out = vec![("session".into(), format!("{desktop} ({session})"))];
    if is_kde() {
        let granted = kwin::available();
        out.push((
            "virtual monitors".into(),
            if granted {
                "KWin direct (exact size and scale, no dialog)".into()
            } else {
                "portal (run `silkscreen host` once to install the KWin desktop entry)".into()
            },
        ));
    } else {
        out.push((
            "virtual monitors".into(),
            "xdg-desktop-portal ScreenCast (virtual source)".into(),
        ));
    }
    out
}

/// Looks for a direct-touch multitouch device among input devices.
pub fn touch_points() -> u8 {
    // INPUT_PROP_DIRECT (bit 1) marks touchscreens as opposed to touchpads.
    let Ok(list) = std::fs::read_to_string("/proc/bus/input/devices") else {
        return 0;
    };
    let direct = list.split("\n\n").any(|dev| {
        dev.lines().any(|l| {
            l.strip_prefix("B: PROP=")
                .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
                .is_some_and(|p| p & 0b10 != 0)
        }) && dev.to_lowercase().contains("touch")
    });
    if direct { 10 } else { 0 }
}
