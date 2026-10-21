//! DXGI Desktop Duplication capture of one monitor.
//!
//! Desktop Duplication does not include the mouse cursor in frames, so it is
//! composited here from the pointer shape DXGI reports — otherwise the remote
//! screen would have no visible cursor at all.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAP_READ_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
    IDXGIAdapter1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use super::Monitor;
use crate::{Capturer, Frame, PixelLayout};

pub struct DxgiCapturer {
    monitor: Monitor,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dup: Option<IDXGIOutputDuplication>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    cursor: Cursor,
    /// Whether `staging` holds a complete frame we can redraw the cursor on.
    have_frame: bool,
}

// COM objects here are only used from the capture thread.
unsafe impl Send for DxgiCapturer {}

impl DxgiCapturer {
    pub fn new(monitor: Monitor) -> Result<Self> {
        let (device, context) = create_device(&monitor.adapter)?;
        let mut c = DxgiCapturer {
            monitor,
            device,
            context,
            dup: None,
            staging: None,
            cursor: Cursor::default(),
            have_frame: false,
        };
        c.reopen()?;
        Ok(c)
    }

    fn reopen(&mut self) -> Result<()> {
        self.dup = None;
        self.have_frame = false;
        let output: IDXGIOutput1 = self.monitor.output.cast()?;
        let dup = unsafe { output.DuplicateOutput(&self.device) }
            .with_context(|| format!("DuplicateOutput on {}", self.monitor.device_name))?;
        let desc = unsafe { dup.GetDesc() };
        info!(
            "duplicating {} ({}x{})",
            self.monitor.device_name, desc.ModeDesc.Width, desc.ModeDesc.Height
        );
        self.dup = Some(dup);
        Ok(())
    }
}

fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((
        device.context("no D3D11 device")?,
        context.context("no D3D11 context")?,
    ))
}

impl Capturer for DxgiCapturer {
    fn next_frame(&mut self, timeout: Duration, f: &mut dyn FnMut(Frame<'_>)) -> Result<bool> {
        if self.dup.is_none() {
            // Lost earlier (mode change, secure desktop); keep trying.
            if let Err(e) = self.reopen() {
                debug!("duplication unavailable: {e:#}");
                std::thread::sleep(timeout);
                return Ok(false);
            }
        }
        let dup = self.dup.clone().unwrap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(timeout.as_millis() as u32, &mut info, &mut resource) }
        {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                warn!("desktop duplication lost (mode change?); reopening");
                self.dup = None;
                return Ok(false);
            }
            Err(e) => {
                self.dup = None;
                bail!("AcquireNextFrame: {e}");
            }
        }

        let result = (|| -> Result<bool> {
            let new_image = info.AccumulatedFrames > 0 && info.LastPresentTime != 0;
            let cursor_moved = info.LastMouseUpdateTime != 0;
            if cursor_moved {
                self.cursor.visible = info.PointerPosition.Visible.as_bool();
                self.cursor.pos = (
                    info.PointerPosition.Position.x,
                    info.PointerPosition.Position.y,
                );
            }
            if info.PointerShapeBufferSize > 0 {
                self.cursor
                    .update_shape(&dup, info.PointerShapeBufferSize)?;
            }
            if !new_image && !(cursor_moved && self.have_frame) && info.PointerShapeBufferSize == 0
            {
                return Ok(false);
            }
            if new_image {
                let texture: ID3D11Texture2D =
                    resource.as_ref().context("no frame resource")?.cast()?;
                let staging = self.staging_for(&texture)?;
                unsafe { self.context.CopyResource(&staging, &texture) };
                self.cursor.saved = None; // fresh image: nothing to restore
                self.have_frame = true;
            }
            Ok(self.have_frame)
        })();
        unsafe {
            let _ = dup.ReleaseFrame();
        }
        if !result? {
            return Ok(false);
        }

        let (staging, w, h) = self.staging.clone().unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut mapped))?
        };
        let pitch = mapped.RowPitch as usize;
        let data =
            unsafe { std::slice::from_raw_parts_mut(mapped.pData as *mut u8, pitch * h as usize) };
        self.cursor.restore(data, pitch);
        self.cursor.draw(data, pitch, w, h);
        f(Frame {
            width: w,
            height: h,
            stride: pitch,
            layout: PixelLayout::Bgrx,
            data,
        });
        unsafe { self.context.Unmap(&staging, 0) };
        Ok(true)
    }
}

impl DxgiCapturer {
    fn staging_for(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut desc) };
        if let Some((t, w, h)) = &self.staging
            && (*w, *h) == (desc.Width, desc.Height)
        {
            return Ok(t.clone());
        }
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32;
        desc.MiscFlags = 0;
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        let tex = tex.context("CreateTexture2D returned nothing")?;
        self.staging = Some((tex.clone(), desc.Width, desc.Height));
        Ok(tex)
    }
}

#[derive(Default)]
struct Cursor {
    visible: bool,
    pos: (i32, i32),
    shape: Vec<u8>,
    info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    /// Pixels under the last drawn cursor: (x, y, w, h, bytes).
    saved: Option<(usize, usize, usize, usize, Vec<u8>)>,
}

impl Cursor {
    fn update_shape(&mut self, dup: &IDXGIOutputDuplication, size: u32) -> Result<()> {
        self.shape.resize(size as usize, 0);
        let mut required = 0;
        unsafe {
            dup.GetFramePointerShape(
                size,
                self.shape.as_mut_ptr() as _,
                &mut required,
                &mut self.info,
            )?;
        }
        Ok(())
    }

    fn restore(&mut self, frame: &mut [u8], pitch: usize) {
        if let Some((x, y, w, h, bytes)) = self.saved.take() {
            for row in 0..h {
                let dst = (y + row) * pitch + x * 4;
                frame[dst..dst + w * 4].copy_from_slice(&bytes[row * w * 4..(row + 1) * w * 4]);
            }
        }
    }

    fn draw(&mut self, frame: &mut [u8], pitch: usize, fw: u32, fh: u32) {
        if !self.visible || self.shape.is_empty() {
            return;
        }
        let ty = self.info.Type as i32;
        let sw = self.info.Width as i32;
        let sh = if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
            self.info.Height as i32 / 2
        } else {
            self.info.Height as i32
        };
        let (x0, y0) = (self.pos.0.max(0), self.pos.1.max(0));
        let (x1, y1) = (
            (self.pos.0 + sw).min(fw as i32),
            (self.pos.1 + sh).min(fh as i32),
        );
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
        let mut saved = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            let s = (y0 as usize + row) * pitch + x0 as usize * 4;
            saved.extend_from_slice(&frame[s..s + w * 4]);
        }
        self.saved = Some((x0 as usize, y0 as usize, w, h, saved));

        let spitch = self.info.Pitch as usize;
        for y in y0..y1 {
            let sy = (y - self.pos.1) as usize;
            for x in x0..x1 {
                let sx = (x - self.pos.0) as usize;
                let d = y as usize * pitch + x as usize * 4;
                let dst = &mut frame[d..d + 3];
                if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 {
                    let s = &self.shape[sy * spitch + sx * 4..sy * spitch + sx * 4 + 4];
                    let a = s[3] as u32;
                    for c in 0..3 {
                        dst[c] = ((s[c] as u32 * a + dst[c] as u32 * (255 - a)) / 255) as u8;
                    }
                } else if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 {
                    let s = &self.shape[sy * spitch + sx * 4..sy * spitch + sx * 4 + 4];
                    if s[3] == 0 {
                        dst.copy_from_slice(&s[..3]);
                    } else {
                        for c in 0..3 {
                            dst[c] ^= s[c];
                        }
                    }
                } else if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
                    let bit = 0x80u8 >> (sx % 8);
                    let and = self.shape[sy * spitch + sx / 8] & bit != 0;
                    let xor = self.shape[(sy + sh as usize) * spitch + sx / 8] & bit != 0;
                    match (and, xor) {
                        (false, false) => dst.fill(0),
                        (false, true) => dst.fill(255),
                        (true, true) => dst.iter_mut().for_each(|c| *c ^= 255),
                        (true, false) => {}
                    }
                }
            }
        }
    }
}
