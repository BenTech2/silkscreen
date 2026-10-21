//! macOS: virtual monitors via `CGVirtualDisplay` (the API behind tools like
//! BetterDisplay and DeskPad), capture via ScreenCaptureKit, and input via
//! CGEvent. macOS has no public touch-injection API, so touch and pen drive
//! the pointer.

mod capture;
mod input;
mod virtual_display;

use anyhow::{Context, Result};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGGetActiveDisplayList, CGMainDisplayID,
};

use crate::{Screen, ScreenRequest, SourceKind};

fn active_displays() -> Vec<CGDirectDisplayID> {
    let mut ids = [0 as CGDirectDisplayID; 32];
    let mut count = 0u32;
    unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    ids[..count as usize].to_vec()
}

fn describe(id: CGDirectDisplayID) -> String {
    let b = CGDisplayBounds(id);
    format!(
        "display {id} {}x{} pt at {},{}",
        b.size.width, b.size.height, b.origin.x, b.origin.y
    )
}

pub fn open_screen(req: &ScreenRequest) -> Result<Screen> {
    let (display_id, guard) = match &req.kind {
        SourceKind::Virtual => {
            let vd = virtual_display::VirtualDisplay::create(req)?;
            (vd.id(), Some(vd))
        }
        SourceKind::Monitor(wanted) => {
            let all = active_displays();
            let list = all
                .iter()
                .map(|&d| describe(d))
                .collect::<Vec<_>>()
                .join(", ");
            let main = CGMainDisplayID();
            let id = match wanted {
                Some(w) => all.iter().copied().find(|d| d.to_string() == *w),
                None => {
                    let others: Vec<_> = all.iter().copied().filter(|&d| d != main).collect();
                    if others.len() == 1 {
                        Some(others[0])
                    } else {
                        None
                    }
                }
            }
            .with_context(|| format!("pick a display id with --monitor; available: {list}"))?;
            (id, None)
        }
        SourceKind::TestPattern => unreachable!(),
    };
    let description = format!(
        "{} ({})",
        describe(display_id),
        if guard.is_some() {
            "virtual display"
        } else {
            "existing display"
        }
    );
    let capturer = capture::SckCapturer::start(display_id, req, guard)?;
    Ok(Screen {
        capturer: Box::new(capturer),
        input: Box::new(input::MacInput::new(display_id)),
        description,
    })
}

pub fn diagnostics() -> Vec<(String, String)> {
    let mut out = vec![(
        "virtual displays".into(),
        if virtual_display::available() {
            "CGVirtualDisplay".into()
        } else {
            "unavailable (macOS 11+ required)".into()
        },
    )];
    for d in active_displays() {
        out.push(("display".into(), describe(d)));
    }
    out
}
