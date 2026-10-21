//! wgpu renderer: uploads decoded YUV planes and draws them aspect-fitted,
//! with the egui interface (home screen, pairing code, debug overlay) on top.
//!
//! Uploads happen on the decode thread (wgpu's `Queue` is thread-safe), so
//! the UI thread only records one draw call per presented frame.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use silkscreen_codec::{Matrix, Picture, PictureFormat};
use winit::window::Window;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    rows: [[f32; 4]; 3],
    mode: u32,
    linearize: u32,
    _pad: [u32; 2],
}

fn yuv_to_rgb_rows(matrix: Matrix, full_range: bool) -> [[f32; 4]; 3] {
    // Cb/Cr contributions to R, G, B.
    let (kr_cr, kg_cb, kg_cr, kb_cb) = match matrix {
        Matrix::Bt709 => (1.5748, -0.1873, -0.4681, 1.8556),
        Matrix::Bt601 => (1.402, -0.344_136, -0.714_136, 1.772),
    };
    let (ys, yo, cs) = if full_range {
        (1.0, 0.0, 1.0)
    } else {
        (255.0 / 219.0, 16.0 / 255.0, 255.0 / 224.0)
    };
    let co = 128.0 / 255.0;
    let row = |kcb: f32, kcr: f32| {
        [
            ys,
            cs * kcb,
            cs * kcr,
            -ys * yo - cs * kcb * co - cs * kcr * co,
        ]
    };
    [row(0.0, kr_cr), row(kg_cb, kg_cr), row(kb_cb, 0.0)]
}

struct VideoTextures {
    format: PictureFormat,
    width: u32,
    height: u32,
    planes: [wgpu::Texture; 3],
    params: wgpu::Buffer,
    color: (Matrix, bool),
    bind_group: wgpu::BindGroup,
}

/// Shared between the decode thread (writes) and the UI thread (draws).
pub struct VideoUpload {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    linearize: bool,
    textures: Mutex<Option<VideoTextures>>,
    /// Set by `upload`, cleared by the UI when it schedules a redraw.
    pub dirty: AtomicBool,
}

impl VideoUpload {
    pub fn upload(&self, pic: &Picture<'_>) {
        let mut guard = self.textures.lock().unwrap();
        let fits = guard
            .as_ref()
            .is_some_and(|t| (t.format, t.width, t.height) == (pic.format, pic.width, pic.height));
        if !fits {
            *guard = Some(self.create_textures(pic));
        }
        let t = guard.as_mut().unwrap();
        if t.color != (pic.matrix, pic.full_range) {
            t.color = (pic.matrix, pic.full_range);
            self.write_params(t);
        }
        let (cw, ch) = (pic.width.div_ceil(2), pic.height.div_ceil(2));
        for (i, &(data, stride)) in pic.planes.iter().enumerate() {
            let (w, h) = if i == 0 {
                (pic.width, pic.height)
            } else {
                (cw, ch)
            };
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &t.planes[i],
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride as u32),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        }
        self.dirty.store(true, Ordering::Release);
    }

    pub fn clear(&self) {
        *self.textures.lock().unwrap() = None;
        self.dirty.store(true, Ordering::Release);
    }

    fn create_textures(&self, pic: &Picture<'_>) -> VideoTextures {
        let tex = |label, w: u32, h: u32, format| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w.max(1),
                    height: h.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let (cw, ch) = (pic.width.div_ceil(2), pic.height.div_ceil(2));
        let planes = match pic.format {
            PictureFormat::Nv12 => [
                tex("y", pic.width, pic.height, wgpu::TextureFormat::R8Unorm),
                tex("uv", cw, ch, wgpu::TextureFormat::Rg8Unorm),
                tex("unused", 1, 1, wgpu::TextureFormat::R8Unorm),
            ],
            PictureFormat::I420 => [
                tex("y", pic.width, pic.height, wgpu::TextureFormat::R8Unorm),
                tex("u", cw, ch, wgpu::TextureFormat::R8Unorm),
                tex("v", cw, ch, wgpu::TextureFormat::R8Unorm),
            ],
        };
        let params = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("yuv params"),
            size: size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let views: Vec<_> = planes
            .iter()
            .map(|t| t.create_view(&Default::default()))
            .collect();
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&views[2]),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: params.as_entire_binding(),
                },
            ],
        });
        let t = VideoTextures {
            format: pic.format,
            width: pic.width,
            height: pic.height,
            planes,
            params,
            color: (pic.matrix, pic.full_range),
            bind_group,
        };
        self.write_params(&t);
        t
    }

    fn write_params(&self, t: &VideoTextures) {
        let p = Params {
            rows: yuv_to_rgb_rows(t.color.0, t.color.1),
            mode: (t.format == PictureFormat::I420) as u32,
            linearize: self.linearize as u32,
            _pad: [0; 2],
        };
        self.queue
            .write_buffer(&t.params, 0, bytemuck::bytes_of(&p));
    }
}

/// Rectangle (x, y, w, h) in window pixels where the video is drawn.
#[derive(Debug, Clone, Copy, Default)]
pub struct VideoRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl VideoRect {
    /// Maps a window position to normalised video coordinates, or `None`
    /// when outside the picture (letterbox bars).
    pub fn normalise(&self, px: f64, py: f64) -> Option<(f32, f32)> {
        if self.w <= 0.0 || self.h <= 0.0 {
            return None;
        }
        let x = (px as f32 - self.x) / self.w;
        let y = (py as f32 - self.y) / self.h;
        ((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)).then_some((x, y))
    }

    pub fn normalise_clamped(&self, px: f64, py: f64) -> (f32, f32) {
        let x = (px as f32 - self.x) / self.w.max(1.0);
        let y = (py as f32 - self.y) / self.h.max(1.0);
        (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
    }
}

fn present_mode(modes: &[wgpu::PresentMode], vsync: bool) -> wgpu::PresentMode {
    if !vsync && modes.contains(&wgpu::PresentMode::Immediate) {
        wgpu::PresentMode::Immediate
    } else if modes.contains(&wgpu::PresentMode::Mailbox) {
        // New frames replace queued ones: no tearing, no queueing delay.
        wgpu::PresentMode::Mailbox
    } else {
        wgpu::PresentMode::Fifo
    }
}

/// Picks the graphics API and GPU. On Windows that is Direct3D 12 first:
/// some Vulkan drivers (older Intel ones, e.g. HD Graphics 620 on a Surface)
/// crash the process while creating the device. Vulkan is the fallback, and
/// `WGPU_BACKEND=vulkan|dx12|gl` overrides the choice everywhere.
fn adapter(
    window: &Arc<Window>,
) -> Result<(wgpu::Instance, wgpu::Surface<'static>, wgpu::Adapter)> {
    let from_env = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    let tries = if cfg!(windows) && std::env::var_os("WGPU_BACKEND").is_none() {
        vec![wgpu::Backends::DX12, wgpu::Backends::VULKAN]
    } else {
        vec![from_env.backends]
    };
    let mut last = None;
    for backends in tries {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..wgpu::InstanceDescriptor::new_without_display_handle_from_env()
        });
        let surface = match instance.create_surface(window.clone()) {
            Ok(s) => s,
            Err(e) => {
                last = Some(anyhow::anyhow!("creating surface ({backends:?}): {e}"));
                continue;
            }
        };
        match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        })) {
            Ok(adapter) => return Ok((instance, surface, adapter)),
            Err(e) => {
                tracing::info!("no {backends:?} GPU: {e}");
                last = Some(anyhow::anyhow!("no GPU adapter ({backends:?}): {e}"));
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no GPU adapter")))
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    present_modes: Vec<wgpu::PresentMode>,
    pipeline: wgpu::RenderPipeline,
    egui: egui_wgpu::Renderer,
    pub video: Arc<VideoUpload>,
    pub rect: VideoRect,
}

/// One frame of egui output to draw over the video.
pub struct UiFrame {
    pub primitives: Vec<egui::ClippedPrimitive>,
    pub textures: egui::TexturesDelta,
    pub pixels_per_point: f32,
}

impl Renderer {
    pub fn new(window: Arc<Window>, vsync: bool) -> Result<Self> {
        let (instance, surface, adapter) = adapter(&window)?;
        tracing::info!(
            "GPU: {} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("silkscreen"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .context("creating device")?;
        tracing::info!("GPU device ready");
        drop(instance);

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        // Video is already gamma-encoded, so prefer a non-sRGB swapchain.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let present_mode = present_mode(&caps.present_modes, vsync);
        tracing::info!("swapchain {format:?}, {present_mode:?}");
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface unsupported by adapter")?;
        config.format = format;
        config.present_mode = present_mode;
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("video"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(format.into())],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let egui = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        Ok(Renderer {
            surface,
            config,
            present_modes: caps.present_modes,
            pipeline,
            egui,
            video: Arc::new(VideoUpload {
                device,
                queue,
                layout,
                sampler,
                linearize: format.is_srgb(),
                textures: Mutex::new(None),
                dirty: AtomicBool::new(true),
            }),
            rect: VideoRect::default(),
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.video.device, &self.config);
    }

    /// Switches between waiting for vertical sync and presenting immediately.
    pub fn set_vsync(&mut self, vsync: bool) {
        let mode = present_mode(&self.present_modes, vsync);
        if mode != self.config.present_mode {
            tracing::info!("swapchain {mode:?}");
            self.config.present_mode = mode;
            self.surface.configure(&self.video.device, &self.config);
        }
    }

    pub fn max_texture_side(&self) -> usize {
        self.video.device.limits().max_texture_dimension_2d as usize
    }

    /// Draws a frame. Returns false if the window is hidden, so the caller
    /// can stop redrawing (presenting is what normally paces the redraws).
    pub fn render(&mut self, ui: Option<UiFrame>) -> bool {
        self.video.dirty.store(false, Ordering::Release);
        let (device, queue) = (&self.video.device, &self.video.queue);
        // Upload texture changes even if this frame can't be drawn: egui
        // sends each one once (the font atlas only on the first frame), and
        // macOS often has no surface texture for a new window's first frame.
        if let Some(ui) = &ui {
            for (id, deltas) in &ui.textures.set {
                for delta in deltas {
                    self.egui.update_texture(device, queue, *id, delta);
                }
            }
        }
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.video.device, &self.config);
                self.free_textures(ui);
                return true;
            }
            // Hidden (covered, minimised, screen locked): nothing to draw
            // into, and no refresh to wait for.
            _ => {
                self.free_textures(ui);
                return false;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self
            .video
            .device
            .create_command_encoder(&Default::default());
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: ui.as_ref().map_or(1.0, |u| u.pixels_per_point),
        };
        let mut extra = Vec::new();
        if let Some(ui) = &ui {
            extra = self
                .egui
                .update_buffers(device, queue, &mut enc, &ui.primitives, &screen);
        }
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("video"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            let textures = self.video.textures.lock().unwrap();
            if let Some(t) = textures.as_ref() {
                self.rect = fit(t.width, t.height, self.config.width, self.config.height);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &t.bind_group, &[]);
                pass.set_viewport(self.rect.x, self.rect.y, self.rect.w, self.rect.h, 0.0, 1.0);
                pass.draw(0..3, 0..1);
            }
            drop(textures);
            if let Some(ui) = &ui {
                // The UI covers the whole window, not just the video rectangle.
                pass.set_viewport(
                    0.0,
                    0.0,
                    self.config.width as f32,
                    self.config.height as f32,
                    0.0,
                    1.0,
                );
                self.egui
                    .render(&mut pass.forget_lifetime(), &ui.primitives, &screen);
            }
        }
        extra.push(enc.finish());
        self.video.queue.submit(extra);
        self.video.queue.present(frame);
        self.free_textures(ui);
        true
    }

    fn free_textures(&mut self, ui: Option<UiFrame>) {
        if let Some(ui) = ui {
            for id in &ui.textures.free {
                self.egui.free_texture(id);
            }
        }
    }
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// Largest rectangle with the video's aspect ratio centred in the window.
/// Pixel-exact (1:1) when the window matches the stream, as it does when the
/// host sized the virtual monitor to this panel.
fn fit(vw: u32, vh: u32, ww: u32, wh: u32) -> VideoRect {
    let scale = (ww as f32 / vw as f32).min(wh as f32 / vh as f32);
    let (w, h) = ((vw as f32 * scale).round(), (vh as f32 * scale).round());
    VideoRect {
        x: ((ww as f32 - w) / 2.0).floor(),
        y: ((wh as f32 - h) / 2.0).floor(),
        w,
        h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limited_range_709_white_and_black() {
        let rows = yuv_to_rgb_rows(Matrix::Bt709, false);
        let apply = |y: f32, u: f32, v: f32| rows.map(|r| r[0] * y + r[1] * u + r[2] * v + r[3]);
        for c in apply(16.0 / 255.0, 0.5019608, 0.5019608) {
            assert!(c.abs() < 1e-4);
        }
        for c in apply(235.0 / 255.0, 0.5019608, 0.5019608) {
            assert!((c - 1.0).abs() < 1e-4);
        }
        // Pure red: Y=63, Cb=102, Cr=240.
        let red = apply(63.0 / 255.0, 102.0 / 255.0, 240.0 / 255.0);
        assert!(
            (red[0] - 1.0).abs() < 0.01 && red[1].abs() < 0.01 && red[2].abs() < 0.01,
            "{red:?}"
        );
    }

    #[test]
    fn letterbox() {
        let r = fit(1920, 1080, 1920, 1200);
        assert_eq!((r.x, r.y, r.w, r.h), (0.0, 60.0, 1920.0, 1080.0));
        assert_eq!(r.normalise(960.0, 30.0), None);
        assert_eq!(r.normalise(960.0, 600.0), Some((0.5, 0.5)));
    }
}
