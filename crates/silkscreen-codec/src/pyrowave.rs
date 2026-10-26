//! PyroWave (experimental): Hans-Kristian Arntzen's intra-only wavelet codec,
//! run in GPU compute shaders — Vulkan on Windows and Linux, a native Metal
//! port on Apple Silicon Macs. Every frame is a keyframe, encode and decode
//! take well under a millisecond, and the price is bandwidth: it is meant
//! for a few hundred Mbit/s on Ethernet.
//!
//! Frames go through the CPU on both sides: BGRX is converted to YUV 4:2:0
//! (BT.709, limited range, as for the FFmpeg encoders) and uploaded; the
//! display reads the decoded planes back into a [`Picture`].
//!
//! On the wire each frame is
//! `[width u16][height u16][packet count u32][packet sizes u32…][packets…]`
//! (little endian): PyroWave's own packets are decoded one by one, and the
//! size lets the display create its decoder without a separate handshake.
//! The width's top bit marks 4:4:4 (full-resolution colour); widths stay
//! below 16384, so it is otherwise unused.

use std::ffi::{c_int, c_void};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail, ensure};
use tracing::{info, warn};

use crate::convert::{ChromaLayout, RgbFrame, YuvPlanes, rgb_to_yuv};
use crate::decoder::{Matrix, Picture, PictureFormat};
use crate::encoder::EncoderConfig;

mod ffi {
    use super::*;

    pub type Device = *mut c_void;
    pub type Encoder = *mut c_void;
    pub type Decoder = *mut c_void;
    pub type Result = c_int;
    pub const SUCCESS: Result = 0;
    pub const CHROMA_420: c_int = 0;
    pub const CHROMA_444: c_int = 1;
    pub const CPU_YUV420P: c_int = 1;
    pub const CPU_YUV444P: c_int = 2;

    #[repr(C)]
    pub struct EncoderCreateInfo {
        pub device: Device,
        pub width: c_int,
        pub height: c_int,
        pub chroma: c_int,
    }

    #[repr(C)]
    pub struct DecoderCreateInfo {
        pub device: Device,
        pub width: c_int,
        pub height: c_int,
        pub chroma: c_int,
        /// Vulkan only: the fragment-shader path for weak GPUs.
        #[cfg(not(target_os = "macos"))]
        pub fragment_path: bool,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Packet {
        pub offset: usize,
        pub size: usize,
    }

    #[repr(C)]
    pub struct RateControl {
        pub maximum_bitstream_size: usize,
    }

    #[repr(C)]
    pub struct CpuBuffer {
        pub data: [*mut c_void; 3],
        pub row_stride_in_bytes: [usize; 3],
        pub plane_size_in_bytes: [usize; 3],
        pub width: c_int,
        pub height: c_int,
        pub format: c_int,
    }

    /// Declares PyroWave's C functions. On macOS the Metal port is linked
    /// in. Elsewhere the library is loaded when PyroWave is first used, so a
    /// missing or blocked DLL (Windows Smart App Control refuses unsigned
    /// ones it doesn't know) only makes PyroWave unavailable instead of
    /// stopping the app from starting.
    macro_rules! api {
        ($(fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;)*) => {
            // (The Vulkan-only ones go unused here.)
            #[cfg(target_os = "macos")]
            #[allow(dead_code)]
            unsafe extern "C" {
                $(pub fn $name($($arg: $ty),*) $(-> $ret)?;)*
            }

            #[cfg(not(target_os = "macos"))]
            pub struct Api {
                $($name: unsafe extern "C" fn($($ty),*) $(-> $ret)?,)*
                _lib: libloading::Library,
            }

            #[cfg(not(target_os = "macos"))]
            impl Api {
                pub fn load(lib: libloading::Library) -> std::result::Result<Self, libloading::Error> {
                    unsafe {
                        Ok(Api {
                            $($name: *lib.get(concat!(stringify!($name), "\0").as_bytes())?,)*
                            _lib: lib,
                        })
                    }
                }
            }

            $(
                #[cfg(not(target_os = "macos"))]
                pub unsafe fn $name($($arg: $ty),*) $(-> $ret)? {
                    unsafe { (api().expect("PyroWave library not loaded").$name)($($arg),*) }
                }
            )*
        };
    }

    api! {
        fn pyrowave_create_default_device(device: *mut Device) -> Result;
        fn pyrowave_encoder_create(info: *const EncoderCreateInfo, enc: *mut Encoder) -> Result;
        fn pyrowave_encoder_encode_cpu(
            enc: Encoder,
            input: *const CpuBuffer,
            rc: *const RateControl,
        ) -> Result;
        fn pyrowave_encoder_compute_num_packets(
            enc: Encoder,
            boundary: usize,
            num: *mut usize,
        ) -> Result;
        fn pyrowave_encoder_packetize(
            enc: Encoder,
            packets: *mut Packet,
            boundary: usize,
            out_packets: *mut usize,
            bitstream: *mut c_void,
            size: usize,
        ) -> Result;
        fn pyrowave_encoder_destroy(enc: Encoder);
        fn pyrowave_decoder_create(info: *const DecoderCreateInfo, dec: *mut Decoder) -> Result;
        fn pyrowave_decoder_clear(dec: Decoder);
        fn pyrowave_decoder_push_packet(dec: Decoder, data: *const c_void, size: usize) -> Result;
        fn pyrowave_decoder_decode_is_ready(dec: Decoder, allow_partial: bool) -> bool;
        fn pyrowave_decoder_destroy(dec: Decoder);
        // Vulkan library only (the Metal port decodes through the shim below).
        fn pyrowave_decoder_device_prefers_fragment_path(device: Device) -> bool;
        fn pyrowave_decoder_decode_cpu_buffer_synchronous(
            dec: Decoder,
            out: *const CpuBuffer,
        ) -> Result;
    }

    // The Metal port decodes into textures only; this shim (pyrowave_metal_shim.mm)
    // decodes into shared textures and copies them out.
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        pub fn pyrowave_result_to_string(r: Result) -> *const std::ffi::c_char;
        pub fn silkscreen_pyrowave_readback_new(
            width: c_int,
            height: c_int,
            full_chroma: c_int,
        ) -> *mut c_void;
        pub fn silkscreen_pyrowave_readback_decode(
            readback: *mut c_void,
            dec: Decoder,
            out: *const CpuBuffer,
        ) -> Result;
        pub fn silkscreen_pyrowave_readback_free(readback: *mut c_void);
    }
}

/// Loads the PyroWave library (next to the executable first), once.
#[cfg(not(target_os = "macos"))]
fn api() -> Option<&'static ffi::Api> {
    static API: OnceLock<Option<ffi::Api>> = OnceLock::new();
    API.get_or_init(|| {
        let names: &[&str] = if cfg!(windows) {
            &[
                "libpyrowave-shared-1.dll",
                "pyrowave-shared-1.dll",
                "pyrowave-shared.dll",
            ]
        } else {
            &["libpyrowave-shared.so.0", "libpyrowave-shared.so"]
        };
        let dir = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.to_path_buf()));
        let mut errors = Vec::new();
        for name in names {
            let paths = dir.iter().map(|d| d.join(name)).chain([name.into()]);
            for path in paths {
                match unsafe { libloading::Library::new(&path) }.and_then(ffi::Api::load) {
                    Ok(api) => {
                        info!("loaded {}", path.display());
                        return Some(api);
                    }
                    Err(e) => errors.push(format!("{}: {e}", path.display())),
                }
            }
        }
        warn!("PyroWave library not loaded:\n{}", errors.join("\n"));
        None
    })
    .as_ref()
}

fn check(r: ffi::Result, what: &str) -> Result<()> {
    if r == ffi::SUCCESS {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    let msg = unsafe { std::ffi::CStr::from_ptr(ffi::pyrowave_result_to_string(r)) }
        .to_string_lossy()
        .into_owned();
    #[cfg(not(target_os = "macos"))]
    let msg = format!("error {r}");
    bail!("{what}: {msg}")
}

/// GPU backend name, for logs and the UI.
const BACKEND: &str = if cfg!(target_os = "macos") {
    "Metal"
} else {
    "Vulkan"
};

/// Marks a 4:4:4 frame in the width field of the frame header.
const FULL_CHROMA: u16 = 0x8000;

/// PyroWave's packets, whose size it treats as a guideline (oversized
/// blocks make bigger ones). Silkscreen splits frames into datagrams itself.
const PACKET_BOUNDARY: usize = 16 * 1024;

struct Device(ffi::Device);
// The device is created once and only used to create encoders/decoders.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

static PROBE: OnceLock<Box<dyn Fn() -> bool + Send + Sync>> = OnceLock::new();

/// Has PyroWave tried out somewhere safe (the app runs a copy of itself)
/// before this process touches the GPU driver: a driver that crashes on it
/// then just leaves PyroWave unavailable instead of taking the app down.
pub fn set_probe(probe: impl Fn() -> bool + Send + Sync + 'static) {
    let _ = PROBE.set(Box::new(probe));
}

/// Encodes and decodes a small frame here; what the separate probe runs.
pub fn probe_here() -> bool {
    let r = (|| -> Result<()> {
        let (w, h) = (64u32, 64u32);
        let rgb = vec![128u8; (w * h * 4) as usize];
        let src = RgbFrame {
            width: w,
            height: h,
            stride: (w * 4) as usize,
            layout: crate::convert::RgbLayout::Bgrx,
            data: &rgb,
        };
        let cfg = EncoderConfig {
            codec: silkscreen_proto::Codec::PyroWave,
            width: w,
            height: h,
            refresh_mhz: 60_000,
            bitrate_kbps: default_bitrate_kbps(w, h, 60_000),
            encoder: None,
            chroma444: false,
        };
        let mut frame = Vec::new();
        PyroEncoder::open(&cfg)?.encode(&src, |d, _| frame = d.to_vec())?;
        let mut dec = PyroDecoder::open()?;
        dec.decode(&frame)?.context("no picture")?;
        Ok(())
    })();
    if let Err(e) = &r {
        warn!("PyroWave check failed: {e:#}");
    }
    r.is_ok()
}

/// The shared GPU device, created on first use (that compiles the shaders),
/// or `None` if this machine can't run PyroWave.
fn device() -> Option<ffi::Device> {
    static DEVICE: OnceLock<Option<Device>> = OnceLock::new();
    DEVICE
        .get_or_init(|| {
            if let Some(probe) = PROBE.get()
                && !probe()
            {
                warn!("PyroWave failed its check on this GPU; leaving it off");
                return None;
            }
            #[cfg(not(target_os = "macos"))]
            api()?;
            let mut dev = std::ptr::null_mut();
            match check(
                unsafe { ffi::pyrowave_create_default_device(&mut dev) },
                "creating the PyroWave device",
            ) {
                Ok(()) => {
                    info!("PyroWave available ({BACKEND})");
                    Some(Device(dev))
                }
                Err(e) => {
                    warn!("PyroWave unavailable: {e:#}");
                    None
                }
            }
        })
        .as_ref()
        .map(|d| d.0)
}

/// Whether this machine's GPU can run PyroWave.
pub fn available() -> bool {
    device().is_some()
}

/// Encoder name shown in the UI and `doctor`.
pub const ENCODER_NAME: &str = "pyrowave";

/// Default bitrate: about 1 bit per pixel, which PyroWave needs to look clean.
pub fn default_bitrate_kbps(width: u32, height: u32, refresh_mhz: u32) -> u32 {
    let fps = refresh_mhz as f64 / 1000.0;
    let kbps = width as f64 * height as f64 * fps / 1000.0;
    (kbps as u32).clamp(50_000, 900_000)
}

/// Planar YUV (4:2:0, or 4:4:4) in one allocation, in the layout
/// PyroWave's CPU calls use.
struct Planes {
    data: Vec<u8>,
    width: usize,
    height: usize,
    full_chroma: bool,
}

impl Planes {
    fn new(width: usize, height: usize, full_chroma: bool) -> Self {
        let chroma = if full_chroma {
            width * height
        } else {
            width * height / 4
        };
        Planes {
            data: vec![0; width * height + 2 * chroma],
            width,
            height,
            full_chroma,
        }
    }

    /// Width (= row stride) of the U and V planes.
    fn chroma_width(&self) -> usize {
        if self.full_chroma {
            self.width
        } else {
            self.width / 2
        }
    }

    fn layout(&self) -> ChromaLayout {
        if self.full_chroma {
            ChromaLayout::I444
        } else {
            ChromaLayout::I420
        }
    }

    fn split(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        let y = self.width * self.height;
        let c = (self.data.len() - y) / 2;
        let (ya, rest) = self.data.split_at_mut(y);
        let (u, v) = rest.split_at_mut(c);
        (ya, u, &mut v[..c])
    }

    fn cpu_buffer(&mut self) -> ffi::CpuBuffer {
        let (w, h, cw) = (self.width, self.height, self.chroma_width());
        let format = if self.full_chroma {
            ffi::CPU_YUV444P
        } else {
            ffi::CPU_YUV420P
        };
        let (y, u, v) = self.split();
        let c = u.len();
        ffi::CpuBuffer {
            data: [
                y.as_mut_ptr().cast(),
                u.as_mut_ptr().cast(),
                v.as_mut_ptr().cast(),
            ],
            row_stride_in_bytes: [w, cw, cw],
            plane_size_in_bytes: [w * h, c, c],
            width: w as c_int,
            height: h as c_int,
            format,
        }
    }
}

pub struct PyroEncoder {
    enc: ffi::Encoder,
    planes: Planes,
    budget: usize,
    bitstream: Vec<u8>,
    packets: Vec<ffi::Packet>,
    /// The last frame as sent, for [`PyroEncoder::encode_again`].
    frame: Vec<u8>,
}

// Driven by one thread at a time, like the FFmpeg encoders.
unsafe impl Send for PyroEncoder {}

impl PyroEncoder {
    pub fn open(cfg: &EncoderConfig) -> Result<Self> {
        ensure!(
            cfg.width.is_multiple_of(2) && cfg.height.is_multiple_of(2),
            "encoder dimensions must be even, got {}x{}",
            cfg.width,
            cfg.height
        );
        ensure!(
            cfg.width <= 16384 && cfg.height <= 16384,
            "PyroWave frames are at most 16384 pixels across"
        );
        let device = device().context("this GPU can't run PyroWave")?;
        let info = ffi::EncoderCreateInfo {
            device,
            width: cfg.width as c_int,
            height: cfg.height as c_int,
            chroma: if cfg.chroma444 {
                ffi::CHROMA_444
            } else {
                ffi::CHROMA_420
            },
        };
        let mut enc = std::ptr::null_mut();
        check(
            unsafe { ffi::pyrowave_encoder_create(&info, &mut enc) },
            "creating the PyroWave encoder",
        )?;
        let fps = (cfg.refresh_mhz.max(1000) as f64) / 1000.0;
        let budget = ((cfg.bitrate_kbps as f64 * 1000.0 / 8.0 / fps) as usize).max(16 * 1024);
        info!(
            "opened {}x{} PyroWave encoder ({BACKEND}), {} KB per frame",
            cfg.width,
            cfg.height,
            budget / 1000
        );
        Ok(PyroEncoder {
            enc,
            planes: Planes::new(cfg.width as usize, cfg.height as usize, cfg.chroma444),
            budget,
            // The rate control is exact, plus some headroom for packet headers.
            bitstream: vec![0; budget * 2 + 64 * 1024],
            packets: Vec::new(),
            frame: Vec::new(),
        })
    }

    pub fn name(&self) -> &str {
        if cfg!(target_os = "macos") {
            "pyrowave (Metal)"
        } else {
            "pyrowave (Vulkan)"
        }
    }

    /// Encodes one frame; `emit` gets it at once (always a keyframe).
    pub fn encode(&mut self, src: &RgbFrame, mut emit: impl FnMut(&[u8], bool)) -> Result<()> {
        let (w, h) = (self.planes.width, self.planes.height);
        ensure!(
            src.width as usize == w && src.height as usize == h,
            "frame size {}x{} does not match encoder {w}x{h}",
            src.width,
            src.height
        );
        {
            let (cw, layout) = (self.planes.chroma_width(), self.planes.layout());
            let (y, u, v) = self.planes.split();
            let dst = YuvPlanes {
                y,
                y_stride: w,
                u,
                u_stride: cw,
                v: Some((v, cw)),
            };
            rgb_to_yuv(src, dst, layout);
        }
        let input = self.planes.cpu_buffer();
        let rc = ffi::RateControl {
            maximum_bitstream_size: self.budget,
        };
        unsafe {
            check(
                ffi::pyrowave_encoder_encode_cpu(self.enc, &input, &rc),
                "PyroWave encode",
            )?;
            let mut n = 0;
            check(
                ffi::pyrowave_encoder_compute_num_packets(self.enc, PACKET_BOUNDARY, &mut n),
                "PyroWave packet count",
            )?;
            self.packets.resize(n, ffi::Packet::default());
            let mut out = 0;
            check(
                ffi::pyrowave_encoder_packetize(
                    self.enc,
                    self.packets.as_mut_ptr(),
                    PACKET_BOUNDARY,
                    &mut out,
                    self.bitstream.as_mut_ptr().cast(),
                    self.bitstream.len(),
                ),
                "PyroWave packetize",
            )?;
            self.packets.truncate(out);
        }
        let f = &mut self.frame;
        f.clear();
        let flag = if self.planes.full_chroma {
            FULL_CHROMA
        } else {
            0
        };
        f.extend_from_slice(&(w as u16 | flag).to_le_bytes());
        f.extend_from_slice(&(h as u16).to_le_bytes());
        f.extend_from_slice(&(self.packets.len() as u32).to_le_bytes());
        for p in &self.packets {
            f.extend_from_slice(&(p.size as u32).to_le_bytes());
        }
        for p in &self.packets {
            f.extend_from_slice(&self.bitstream[p.offset..p.offset + p.size]);
        }
        emit(&self.frame, true);
        Ok(())
    }

    /// Sends the last frame again (it is a keyframe anyway).
    pub fn encode_again(&mut self, mut emit: impl FnMut(&[u8], bool)) -> Result<()> {
        if !self.frame.is_empty() {
            emit(&self.frame, true);
        }
        Ok(())
    }
}

impl Drop for PyroEncoder {
    fn drop(&mut self) {
        unsafe { ffi::pyrowave_encoder_destroy(self.enc) };
    }
}

/// A decoder for one frame size.
struct Sized {
    dec: ffi::Decoder,
    planes: Planes,
    #[cfg(target_os = "macos")]
    readback: *mut c_void,
}

impl Drop for Sized {
    fn drop(&mut self) {
        unsafe {
            #[cfg(target_os = "macos")]
            ffi::silkscreen_pyrowave_readback_free(self.readback);
            ffi::pyrowave_decoder_destroy(self.dec);
        }
    }
}

pub struct PyroDecoder {
    /// Created from the first frame's size, recreated if it changes.
    sized: Option<Sized>,
}

unsafe impl Send for PyroDecoder {}

impl PyroDecoder {
    pub fn open() -> Result<Self> {
        device().context("this GPU can't run PyroWave")?;
        Ok(PyroDecoder { sized: None })
    }

    pub fn name(&self) -> &str {
        if cfg!(target_os = "macos") {
            "pyrowave (Metal)"
        } else {
            "pyrowave (Vulkan)"
        }
    }

    fn sized(&mut self, width: usize, height: usize, full_chroma: bool) -> Result<&mut Sized> {
        if self.sized.as_ref().is_some_and(|s| {
            (s.planes.width, s.planes.height, s.planes.full_chroma) == (width, height, full_chroma)
        }) {
            return Ok(self.sized.as_mut().unwrap());
        }
        self.sized = None;
        let device = device().context("no PyroWave device")?;
        let info = ffi::DecoderCreateInfo {
            device,
            width: width as c_int,
            height: height as c_int,
            chroma: if full_chroma {
                ffi::CHROMA_444
            } else {
                ffi::CHROMA_420
            },
            #[cfg(not(target_os = "macos"))]
            fragment_path: unsafe { ffi::pyrowave_decoder_device_prefers_fragment_path(device) },
        };
        let mut dec = std::ptr::null_mut();
        check(
            unsafe { ffi::pyrowave_decoder_create(&info, &mut dec) },
            "creating the PyroWave decoder",
        )?;
        #[cfg(target_os = "macos")]
        let readback = unsafe {
            ffi::silkscreen_pyrowave_readback_new(
                width as c_int,
                height as c_int,
                full_chroma as c_int,
            )
        };
        #[cfg(target_os = "macos")]
        if readback.is_null() {
            unsafe { ffi::pyrowave_decoder_destroy(dec) };
            bail!("cannot create Metal textures for PyroWave");
        }
        info!("opened {width}x{height} PyroWave decoder ({BACKEND})");
        Ok(self.sized.insert(Sized {
            dec,
            planes: Planes::new(width, height, full_chroma),
            #[cfg(target_os = "macos")]
            readback,
        }))
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Picture<'_>>> {
        let (width, height, full_chroma, packets) = parse_frame(data)?;
        let s = self.sized(width, height, full_chroma)?;
        unsafe {
            ffi::pyrowave_decoder_clear(s.dec);
            for p in packets {
                check(
                    ffi::pyrowave_decoder_push_packet(s.dec, p.as_ptr().cast(), p.len()),
                    "PyroWave push packet",
                )?;
            }
            if !ffi::pyrowave_decoder_decode_is_ready(s.dec, false) {
                bail!("incomplete PyroWave frame");
            }
            let out = s.planes.cpu_buffer();
            #[cfg(target_os = "macos")]
            let r = ffi::silkscreen_pyrowave_readback_decode(s.readback, s.dec, &out);
            #[cfg(not(target_os = "macos"))]
            let r = ffi::pyrowave_decoder_decode_cpu_buffer_synchronous(s.dec, &out);
            check(r, "PyroWave decode")?;
        }
        let (w, h, cw) = (width, height, s.planes.chroma_width());
        let format = if full_chroma {
            PictureFormat::I444
        } else {
            PictureFormat::I420
        };
        let (y, u, v) = s.planes.split();
        Ok(Some(Picture {
            format,
            width: w as u32,
            height: h as u32,
            matrix: Matrix::Bt709,
            full_range: false,
            planes: vec![(&*y, w), (&*u, cw), (&*v, cw)],
        }))
    }
}

/// Splits a frame into its size, chroma (true: 4:4:4) and PyroWave packets.
fn parse_frame(data: &[u8]) -> Result<(usize, usize, bool, Vec<&[u8]>)> {
    let u32_at = |i: usize| -> Result<usize> {
        let b = data.get(i..i + 4).context("truncated PyroWave frame")?;
        Ok(u32::from_le_bytes(b.try_into().unwrap()) as usize)
    };
    ensure!(data.len() >= 8, "truncated PyroWave frame");
    let raw_width = u16::from_le_bytes([data[0], data[1]]);
    let full_chroma = raw_width & FULL_CHROMA != 0;
    let width = (raw_width & !FULL_CHROMA) as usize;
    let height = u16::from_le_bytes([data[2], data[3]]) as usize;
    ensure!(
        width > 0 && height > 0 && width.is_multiple_of(2) && height.is_multiple_of(2),
        "bad PyroWave frame size {width}x{height}"
    );
    let n = u32_at(4)?;
    ensure!(n <= data.len() / 4, "bad PyroWave packet count {n}");
    let mut at = 8 + 4 * n;
    let mut packets = Vec::with_capacity(n);
    for i in 0..n {
        let size = u32_at(8 + 4 * i)?;
        let p = data
            .get(at..at + size)
            .context("truncated PyroWave packet")?;
        packets.push(p);
        at += size;
    }
    Ok((width, height, full_chroma, packets))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::RgbLayout;
    use silkscreen_proto::Codec;

    #[test]
    fn rejects_garbage() {
        assert!(parse_frame(&[]).is_err());
        assert!(parse_frame(&[2, 0, 2, 0, 255, 255, 255, 255]).is_err());
        assert!(parse_frame(&[2, 0, 2, 0, 1, 0, 0, 0, 9, 0, 0, 0, 1]).is_err());
    }

    /// Encodes and decodes a gradient on this machine's GPU; skipped where
    /// PyroWave can't run (no Vulkan driver, Intel Mac, CI runners).
    #[test]
    fn roundtrip() {
        if !available() {
            eprintln!("PyroWave unavailable here; skipping");
            return;
        }
        let (w, h) = (640u32, 360u32);
        let mut rgb = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                rgb[i] = (x * 255 / w) as u8;
                rgb[i + 1] = (y * 255 / h) as u8;
                rgb[i + 2] = 128;
            }
        }
        let src = RgbFrame {
            width: w,
            height: h,
            stride: (w * 4) as usize,
            layout: RgbLayout::Bgrx,
            data: &rgb,
        };
        let cfg = EncoderConfig {
            codec: Codec::PyroWave,
            width: w,
            height: h,
            refresh_mhz: 60_000,
            bitrate_kbps: default_bitrate_kbps(w, h, 60_000),
            encoder: None,
            chroma444: false,
        };
        let mut enc = PyroEncoder::open(&cfg).unwrap();
        let mut frame = Vec::new();
        enc.encode(&src, |d, key| {
            assert!(key);
            frame = d.to_vec();
        })
        .unwrap();
        let mut dec = PyroDecoder::open().unwrap();
        let pic = dec.decode(&frame).unwrap().unwrap();
        assert_eq!((pic.width, pic.height), (w, h));
        // Compare luma against the converter's own output.
        let mut expect = Planes::new(w as usize, h as usize, false);
        {
            let (y, u, v) = expect.split();
            let dst = YuvPlanes {
                y,
                y_stride: w as usize,
                u,
                u_stride: w as usize / 2,
                v: Some((v, w as usize / 2)),
            };
            rgb_to_yuv(&src, dst, ChromaLayout::I420);
        }
        let (ey, _, _) = expect.split();
        let mse = pic.planes[0]
            .0
            .iter()
            .zip(ey.iter())
            .map(|(a, b)| (*a as f64 - *b as f64).powi(2))
            .sum::<f64>()
            / ey.len() as f64;
        let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
        assert!(psnr > 35.0, "luma PSNR {psnr:.1} dB");
    }

    /// 4:4:4 keeps a 1-pixel red/blue checkerboard's colours apart.
    #[test]
    fn roundtrip_444() {
        if !available() {
            eprintln!("PyroWave unavailable here; skipping");
            return;
        }
        let (w, h) = (128u32, 96u32);
        let mut rgb = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                rgb[i + if (x + y) % 2 == 0 { 2 } else { 0 }] = 220;
            }
        }
        let src = RgbFrame {
            width: w,
            height: h,
            stride: (w * 4) as usize,
            layout: RgbLayout::Bgrx,
            data: &rgb,
        };
        let cfg = EncoderConfig {
            codec: Codec::PyroWave,
            width: w,
            height: h,
            refresh_mhz: 60_000,
            bitrate_kbps: 400_000,
            encoder: None,
            chroma444: true,
        };
        let mut frame = Vec::new();
        PyroEncoder::open(&cfg)
            .unwrap()
            .encode(&src, |d, _| frame = d.to_vec())
            .unwrap();
        let mut dec = PyroDecoder::open().unwrap();
        let pic = dec.decode(&frame).unwrap().unwrap();
        assert_eq!(pic.format, PictureFormat::I444);
        let (v, stride) = pic.planes[2];
        let mut sum = 0u64;
        for y in 0..h as usize {
            for x in 1..w as usize {
                sum += (v[y * stride + x] as i64 - v[y * stride + x - 1] as i64).unsigned_abs();
            }
        }
        let contrast = sum as f64 / ((w as usize - 1) * h as usize) as f64;
        assert!(contrast > 40.0, "colour detail lost: {contrast:.1}");
    }
}
