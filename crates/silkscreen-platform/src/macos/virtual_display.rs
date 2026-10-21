//! `CGVirtualDisplay`: a private but long-stable CoreGraphics API (macOS 11+)
//! that creates a real display the window server treats like a monitor. It
//! disappears when the object is released.

use anyhow::{Context, Result, bail};
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSArray, NSString};
use tracing::info;

use crate::ScreenRequest;

pub fn available() -> bool {
    AnyClass::get(c"CGVirtualDisplay").is_some()
}

fn class(name: &std::ffi::CStr) -> Result<&'static AnyClass> {
    AnyClass::get(name).with_context(|| format!("{name:?} not found (macOS 11+ required)"))
}

unsafe extern "C" {
    // libdispatch, part of libSystem.
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut AnyObject;
}

pub struct VirtualDisplay {
    /// Released on drop, which unplugs the display.
    _display: Retained<AnyObject>,
    id: u32,
}

// The display object is only messaged on creation and release.
unsafe impl Send for VirtualDisplay {}

impl VirtualDisplay {
    pub fn create(req: &ScreenRequest) -> Result<Self> {
        let hidpi = req.scale >= 1.5;
        // Modes are in points; with HiDPI each point is 2x2 pixels.
        let (pw, ph) = if hidpi {
            (req.width / 2, req.height / 2)
        } else {
            (req.width, req.height)
        };
        let hz = req.refresh_mhz as f64 / 1000.0;
        // ~110 DPI unless the display told us its physical size.
        let (mm_w, mm_h) = req
            .physical_size_mm
            .map(|(w, h)| (w as f64, h as f64))
            .unwrap_or((
                req.width as f64 * 25.4 / 110.0,
                req.height as f64 * 25.4 / 110.0,
            ));

        unsafe {
            let desc: Retained<AnyObject> = msg_send![class(c"CGVirtualDisplayDescriptor")?, new];
            let name = NSString::from_str(&req.name);
            let _: () = msg_send![&*desc, setName: &*name];
            let _: () = msg_send![&*desc, setMaxPixelsWide: req.width];
            let _: () = msg_send![&*desc, setMaxPixelsHigh: req.height];
            let _: () =
                msg_send![&*desc, setSizeInMillimeters: CGSize { width: mm_w, height: mm_h }];
            let _: () = msg_send![&*desc, setVendorID: 0x5153u32]; // "SS"
            let _: () = msg_send![&*desc, setProductID: 0x0001u32];
            let _: () = msg_send![&*desc, setSerialNum: 0x0001u32];
            let queue = dispatch_get_global_queue(0, 0);
            let _: () = msg_send![&*desc, setQueue: queue];

            let alloc: Allocated<AnyObject> = msg_send![class(c"CGVirtualDisplay")?, alloc];
            let display: Option<Retained<AnyObject>> = msg_send![alloc, initWithDescriptor: &*desc];
            let display = display.context("CGVirtualDisplay initWithDescriptor failed")?;

            let mode_alloc: Allocated<AnyObject> =
                msg_send![class(c"CGVirtualDisplayMode")?, alloc];
            let mode: Option<Retained<AnyObject>> = msg_send![mode_alloc, initWithWidth: pw as usize, height: ph as usize, refreshRate: hz];
            let mode = mode.context("CGVirtualDisplayMode init failed")?;
            let settings: Retained<AnyObject> = msg_send![class(c"CGVirtualDisplaySettings")?, new];
            let _: () = msg_send![&*settings, setHiDPI: hidpi as u32];
            let modes = NSArray::from_retained_slice(&[mode]);
            let _: () = msg_send![&*settings, setModes: &*modes];
            let ok: bool = msg_send![&*display, applySettings: &*settings];
            if !ok {
                bail!("CGVirtualDisplay rejected {pw}x{ph} @ {hz} Hz");
            }
            let id: u32 = msg_send![&*display, displayID];
            info!(
                "created virtual display {id}: {pw}x{ph} pt{} @ {hz:.2} Hz",
                if hidpi { " (HiDPI)" } else { "" }
            );
            // Give the window server a moment to bring the display online.
            std::thread::sleep(std::time::Duration::from_millis(500));
            Ok(VirtualDisplay {
                _display: display,
                id,
            })
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        info!("removing virtual display {}", self.id);
    }
}
