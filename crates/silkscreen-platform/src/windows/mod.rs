//! Windows: virtual monitors via the Parsec Virtual Display Driver (or any
//! existing monitor such as a dummy plug), DXGI Desktop Duplication capture,
//! and synthetic pointer injection for touch and pen.

mod dxgi;
mod input;
mod vdd;

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
};
use windows::Win32::Graphics::Gdi::{
    CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
    DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_MAXIMUMTOUCHES};
use windows::core::PCWSTR;

use crate::{Capturer, Frame, Screen, ScreenRequest, SourceKind};

/// One DXGI output (monitor) and the adapter that scans it out.
#[derive(Clone)]
pub struct Monitor {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput,
    device_name: String,
    rect: RECT,
}

fn monitors() -> Result<Vec<Monitor>> {
    let mut out = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                o += 1;
                let desc = output.GetDesc()?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let name = String::from_utf16_lossy(&desc.DeviceName);
                out.push(Monitor {
                    adapter: adapter.clone(),
                    output,
                    device_name: name.trim_end_matches('\0').to_string(),
                    rect: desc.DesktopCoordinates,
                });
            }
        }
    }
    Ok(out)
}

/// Current desktop rectangle of a monitor by GDI device name (NUL-terminated UTF-16).
fn monitor_rect(device: &[u16]) -> Option<RECT> {
    unsafe {
        let mut dm = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !EnumDisplaySettingsW(PCWSTR(device.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool()
        {
            return None;
        }
        let pos = dm.Anonymous1.Anonymous2.dmPosition;
        Some(RECT {
            left: pos.x,
            top: pos.y,
            right: pos.x + dm.dmPelsWidth as i32,
            bottom: pos.y + dm.dmPelsHeight as i32,
        })
    }
}

fn set_mode(device_name: &str, width: u32, height: u32, hz: u32) {
    let name: Vec<u16> = device_name.encode_utf16().chain([0]).collect();
    let dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
        dmPelsWidth: width,
        dmPelsHeight: height,
        dmDisplayFrequency: hz,
        ..Default::default()
    };
    let r = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(name.as_ptr()),
            Some(&dm),
            None,
            CDS_UPDATEREGISTRY,
            None,
        )
    };
    if r == DISP_CHANGE_SUCCESSFUL {
        info!("set {device_name} to {width}x{height} @ {hz} Hz");
    } else {
        warn!(
            "could not set {device_name} to {width}x{height} @ {hz} Hz ({r:?}); using its current mode"
        );
    }
}

pub fn alert(title: &str, text: &str) {
    use ::windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use ::windows::core::HSTRING;
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(title),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub fn monitor_list() -> Vec<crate::MonitorInfo> {
    monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| crate::MonitorInfo {
            id: m.device_name.clone(),
            description: describe(m),
            main: (m.rect.left, m.rect.top) == (0, 0),
        })
        .collect()
}

fn describe(m: &Monitor) -> String {
    let r = m.rect;
    format!(
        "{} {}x{} at {},{}",
        m.device_name,
        r.right - r.left,
        r.bottom - r.top,
        r.left,
        r.top
    )
}

/// Keeps a value (the plugged-in virtual display) alive as long as the capturer.
struct Guarded<C> {
    inner: C,
    _guard: Option<vdd::VirtualDisplay>,
}

impl<C: Capturer> Capturer for Guarded<C> {
    fn next_frame(&mut self, timeout: Duration, f: &mut dyn FnMut(Frame<'_>)) -> Result<bool> {
        self.inner.next_frame(timeout, f)
    }
}

pub fn open_screen(req: &ScreenRequest) -> Result<Screen> {
    // Physical pixel coordinates everywhere, regardless of scaling.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let hz = (req.refresh_mhz as f32 / 1000.0).round() as u32;

    let (monitor, guard) = match &req.kind {
        SourceKind::Virtual => {
            vdd::ensure_installed()?;
            let before: Vec<String> = monitors()?.into_iter().map(|m| m.device_name).collect();
            let vd = vdd::VirtualDisplay::plug(req.width, req.height, hz)?;
            let deadline = Instant::now() + Duration::from_secs(8);
            let monitor = loop {
                if let Some(m) = monitors()?
                    .into_iter()
                    .find(|m| !before.contains(&m.device_name))
                {
                    break m;
                }
                if Instant::now() > deadline {
                    bail!("the virtual display did not appear");
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            set_mode(&monitor.device_name, req.width, req.height, hz);
            // Re-read: the output's coordinates change with the mode.
            let name = monitor.device_name.clone();
            let monitor = monitors()?
                .into_iter()
                .find(|m| m.device_name == name)
                .unwrap_or(monitor);
            (monitor, Some(vd))
        }
        SourceKind::Monitor(wanted) => {
            let all = monitors()?;
            let list = all.iter().map(describe).collect::<Vec<_>>().join(", ");
            let chosen = match wanted {
                // Exact first: \\.\DISPLAY1 is also part of \\.\DISPLAY10.
                Some(w) => {
                    let w = w.to_lowercase();
                    let exact = all.iter().position(|m| m.device_name.to_lowercase() == w);
                    let i = exact.or_else(|| {
                        all.iter()
                            .position(|m| m.device_name.to_lowercase().contains(&w))
                    });
                    i.map(|i| all.into_iter().nth(i).unwrap())
                }
                // Without a name, a lone secondary monitor (dummy plug) is the obvious choice.
                None => {
                    let secondary: Vec<_> = all
                        .into_iter()
                        .filter(|m| (m.rect.left, m.rect.top) != (0, 0))
                        .collect();
                    if secondary.len() == 1 {
                        secondary.into_iter().next()
                    } else {
                        None
                    }
                }
            };
            let m = chosen
                .with_context(|| format!("pick a monitor with --monitor; available: {list}"))?;
            (m, None)
        }
        SourceKind::TestPattern => unreachable!(),
    };

    let description = format!(
        "{} ({})",
        describe(&monitor),
        if guard.is_some() {
            "virtual display"
        } else {
            "existing monitor"
        }
    );
    let input = input::WindowsInput::new(&monitor.device_name);
    let capturer = dxgi::DxgiCapturer::new(monitor)?;
    Ok(Screen {
        capturer: Box::new(Guarded {
            inner: capturer,
            _guard: guard,
        }),
        input: Box::new(input),
        description,
    })
}

pub fn touch_points() -> u8 {
    unsafe { GetSystemMetrics(SM_MAXIMUMTOUCHES) }.clamp(0, 255) as u8
}

pub fn diagnostics() -> Vec<(String, String)> {
    let mut out = vec![(
        "virtual display driver".into(),
        match vdd::version() {
            Some(v) => format!("Parsec VDD (version {v})"),
            None => "not installed (dummy plugs work with --source monitor)".into(),
        },
    )];
    if let Ok(ms) = monitors() {
        for m in ms {
            out.push(("monitor".into(), describe(&m)));
        }
    }
    out
}
