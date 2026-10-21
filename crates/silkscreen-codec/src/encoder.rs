//! Low-latency encoder selection and encoding.

use std::ffi::{CStr, CString};

use anyhow::{Result, anyhow, bail};
use ff::{AVHWDeviceType as Hw, AVPixelFormat as Pix};
use ffmpeg_sys_next as ff;
use silkscreen_proto::Codec;
use tracing::{debug, info};

use crate::convert::{ChromaLayout, RgbFrame, YuvPlanes, rgb_to_yuv420};
use crate::ff::{
    BufRef, CodecCtx, Dict, EAGAIN, Frame, Packet, check, create_hw_device, init_logging,
};

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// Frame rate in millihertz.
    pub refresh_mhz: u32,
    pub bitrate_kbps: u32,
    /// Force a specific FFmpeg encoder (e.g. "h264_nvenc"); otherwise the
    /// best available one is chosen.
    pub encoder: Option<String>,
}

/// Encoders to try, best first, for this platform.
pub fn candidates(codec: Codec) -> Vec<&'static str> {
    let hw: &[&str] = match codec {
        #[cfg(target_os = "linux")]
        Codec::H264 => &["h264_nvenc", "h264_vaapi", "h264_qsv", "h264_amf"],
        #[cfg(target_os = "linux")]
        Codec::Hevc => &["hevc_nvenc", "hevc_vaapi", "hevc_qsv", "hevc_amf"],
        #[cfg(target_os = "linux")]
        Codec::Av1 => &["av1_nvenc", "av1_vaapi", "av1_qsv", "av1_amf"],
        #[cfg(windows)]
        Codec::H264 => &["h264_nvenc", "h264_amf", "h264_qsv", "h264_mf"],
        #[cfg(windows)]
        Codec::Hevc => &["hevc_nvenc", "hevc_amf", "hevc_qsv", "hevc_mf"],
        #[cfg(windows)]
        Codec::Av1 => &["av1_nvenc", "av1_amf", "av1_qsv", "av1_mf"],
        #[cfg(target_os = "macos")]
        Codec::H264 => &["h264_videotoolbox"],
        #[cfg(target_os = "macos")]
        Codec::Hevc => &["hevc_videotoolbox"],
        #[cfg(target_os = "macos")]
        Codec::Av1 => &[],
        #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
        _ => &[],
    };
    let sw: &[&str] = match codec {
        Codec::H264 => &["libx264", "libopenh264"],
        Codec::Hevc => &["libx265"],
        Codec::Av1 => &["libsvtav1"],
    };
    hw.iter().chain(sw).copied().collect()
}

/// Low-latency settings per encoder family. Unknown keys are ignored by
/// FFmpeg, so it is fine to be generous.
fn low_latency_options(name: &str, d: &mut Dict) {
    let family = name.split('_').nth(1).unwrap_or(name);
    match family {
        "nvenc" => {
            d.set("preset", "p1");
            d.set("tune", "ull");
            d.set("rc", "cbr");
            d.set("zerolatency", "1");
            d.set("delay", "0");
            d.set("forced-idr", "1");
            d.set("rc-lookahead", "0");
            d.set("no-scenecut", "1");
            d.set("multipass", "disabled");
        }
        "vaapi" => {
            d.set("rc_mode", "CBR");
            d.set("async_depth", "1");
        }
        "qsv" => {
            d.set("preset", "veryfast");
            d.set("async_depth", "1");
            d.set("look_ahead", "0");
            d.set("forced_idr", "1");
        }
        "amf" => {
            d.set("usage", "ultralowlatency");
            d.set("quality", "speed");
            d.set("rc", "cbr");
            d.set("latency", "1");
        }
        "videotoolbox" => {
            d.set("realtime", "1");
            d.set("prio_speed", "1");
        }
        "mf" => {
            d.set("scenario", "display_remoting");
            d.set("rate_control", "cbr");
            d.set("hw_encoding", "1");
        }
        _ => match name {
            "libx264" | "libx265" => {
                d.set("preset", "superfast");
                d.set("tune", "zerolatency");
                d.set("forced-idr", "1");
            }
            "libsvtav1" => d.set("preset", "12"),
            _ => {}
        },
    }
}

enum Input {
    /// Encoder takes the software frame directly.
    Software,
    /// Encoder needs frames uploaded into a hardware frame pool.
    Upload { frames: BufRef, hw_frame: Frame },
}

pub struct Encoder {
    ctx: CodecCtx,
    name: String,
    chroma: ChromaLayout,
    frame: Frame,
    input: Input,
    pkt: Packet,
    /// Packets produced so far.
    frames_out: u64,
}

// Each encoder is driven by a single thread at a time.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn open(cfg: &EncoderConfig) -> Result<Self> {
        init_logging();
        if !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) {
            bail!(
                "encoder dimensions must be even, got {}x{}",
                cfg.width,
                cfg.height
            );
        }
        let names: Vec<String> = match &cfg.encoder {
            Some(n) => vec![n.clone()],
            None => candidates(cfg.codec)
                .into_iter()
                .map(String::from)
                .collect(),
        };
        let mut errors = Vec::new();
        for name in &names {
            match Self::try_open(name, cfg) {
                Ok(enc) => {
                    info!(encoder = %name, "opened {}x{} encoder", cfg.width, cfg.height);
                    return Ok(enc);
                }
                Err(e) => {
                    debug!(encoder = %name, "unavailable: {e:#}");
                    errors.push(format!("{name}: {e:#}"));
                }
            }
        }
        Err(anyhow!(
            "no usable {} encoder:\n  {}",
            cfg.codec.name(),
            errors.join("\n  ")
        ))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn try_open(name: &str, cfg: &EncoderConfig) -> Result<Self> {
        unsafe {
            let cname = CString::new(name)?;
            let codec = ff::avcodec_find_encoder_by_name(cname.as_ptr());
            if codec.is_null() {
                bail!("not built into this FFmpeg");
            }

            let formats = supported_pix_fmts(codec);
            let (pix_fmt, chroma, hw) =
                if formats.is_empty() || formats.contains(&Pix::AV_PIX_FMT_NV12) {
                    (Pix::AV_PIX_FMT_NV12, ChromaLayout::Nv12, None)
                } else if formats.contains(&Pix::AV_PIX_FMT_YUV420P) {
                    (Pix::AV_PIX_FMT_YUV420P, ChromaLayout::I420, None)
                } else if formats.contains(&Pix::AV_PIX_FMT_VAAPI) {
                    (
                        Pix::AV_PIX_FMT_VAAPI,
                        ChromaLayout::Nv12,
                        Some(Hw::AV_HWDEVICE_TYPE_VAAPI),
                    )
                } else {
                    bail!("no supported input pixel format");
                };

            let ctx = CodecCtx(ff::avcodec_alloc_context3(codec));
            if ctx.0.is_null() {
                bail!("avcodec_alloc_context3 failed");
            }
            let c = &mut *ctx.0;
            let fps = (cfg.refresh_mhz as f64 / 1000.0).max(1.0);
            let bit_rate = cfg.bitrate_kbps as i64 * 1000;
            c.width = cfg.width as i32;
            c.height = cfg.height as i32;
            c.pix_fmt = pix_fmt;
            c.time_base = ff::AVRational {
                num: 1,
                den: 1_000_000,
            };
            c.framerate = ff::AVRational {
                num: cfg.refresh_mhz as i32,
                den: 1000,
            };
            c.bit_rate = bit_rate;
            c.rc_max_rate = bit_rate;
            // A VBV of ~2 frames keeps single frames from bursting far above
            // the link rate, which is what keeps latency flat.
            c.rc_buffer_size = (bit_rate as f64 * 2.0 / fps) as i32;
            // Infinite-ish GOP: keyframes are sent on request (loss) plus a
            // slow periodic refresh as a safety net.
            c.gop_size = (fps * 30.0) as i32;
            c.max_b_frames = 0;
            c.thread_count = 0;
            c.thread_type = ff::FF_THREAD_SLICE;
            c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            c.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;

            let input = match hw {
                None => Input::Software,
                Some(kind) => {
                    let device = create_hw_device(kind)?;
                    let frames = BufRef(ff::av_hwframe_ctx_alloc(device.0));
                    if frames.0.is_null() {
                        bail!("av_hwframe_ctx_alloc failed");
                    }
                    let fc = &mut *((*frames.0).data as *mut ff::AVHWFramesContext);
                    fc.format = pix_fmt;
                    fc.sw_format = Pix::AV_PIX_FMT_NV12;
                    fc.width = c.width;
                    fc.height = c.height;
                    fc.initial_pool_size = 4;
                    check(ff::av_hwframe_ctx_init(frames.0), "av_hwframe_ctx_init")?;
                    c.hw_frames_ctx = ff::av_buffer_ref(frames.0);
                    Input::Upload {
                        frames,
                        hw_frame: Frame::new()?,
                    }
                }
            };

            let mut opts = Dict::new();
            low_latency_options(name, &mut opts);
            check(
                ff::avcodec_open2(ctx.0, codec, &mut opts.0),
                "avcodec_open2",
            )?;

            let frame = Frame::new()?;
            let f = &mut *frame.0;
            f.format = match chroma {
                ChromaLayout::Nv12 => Pix::AV_PIX_FMT_NV12,
                ChromaLayout::I420 => Pix::AV_PIX_FMT_YUV420P,
            } as i32;
            f.width = c.width;
            f.height = c.height;
            f.color_range = c.color_range;
            f.colorspace = c.colorspace;
            f.color_primaries = c.color_primaries;
            f.color_trc = c.color_trc;
            check(ff::av_frame_get_buffer(frame.0, 64), "av_frame_get_buffer")?;

            Ok(Encoder {
                ctx,
                name: name.to_owned(),
                chroma,
                frame,
                input,
                pkt: Packet::new()?,
                frames_out: 0,
            })
        }
    }

    /// Encodes one captured frame. `emit` is called for each output packet
    /// with `(bitstream, is_keyframe)`; with the low-latency settings above
    /// that is exactly once per input frame.
    pub fn encode(
        &mut self,
        src: &RgbFrame,
        pts_us: u64,
        force_keyframe: bool,
        mut emit: impl FnMut(&[u8], bool),
    ) -> Result<()> {
        unsafe {
            check(
                ff::av_frame_make_writable(self.frame.0),
                "av_frame_make_writable",
            )?;
            let f = &mut *self.frame.0;
            if src.width != f.width as u32 || src.height != f.height as u32 {
                bail!(
                    "frame size {}x{} does not match encoder {}x{}",
                    src.width,
                    src.height,
                    f.width,
                    f.height
                );
            }
            let h = f.height as usize;
            let plane = |i: usize, rows: usize| {
                std::slice::from_raw_parts_mut(f.data[i], f.linesize[i] as usize * rows)
            };
            let dst = YuvPlanes {
                y: plane(0, h),
                y_stride: f.linesize[0] as usize,
                u: plane(1, h / 2),
                u_stride: f.linesize[1] as usize,
                v: match self.chroma {
                    ChromaLayout::Nv12 => None,
                    ChromaLayout::I420 => Some((plane(2, h / 2), f.linesize[2] as usize)),
                },
            };
            rgb_to_yuv420(src, dst, self.chroma);
        }
        self.submit(pts_us, force_keyframe, &mut emit)
    }

    /// Re-encodes the previous frame. Used to answer a keyframe request when
    /// the screen is static and no new frame is coming.
    pub fn encode_again(
        &mut self,
        pts_us: u64,
        force_keyframe: bool,
        mut emit: impl FnMut(&[u8], bool),
    ) -> Result<()> {
        self.submit(pts_us, force_keyframe, &mut emit)
    }

    fn submit(
        &mut self,
        pts_us: u64,
        force_keyframe: bool,
        emit: &mut impl FnMut(&[u8], bool),
    ) -> Result<()> {
        unsafe {
            let f = &mut *self.frame.0;
            f.pts = pts_us as i64;
            if force_keyframe {
                f.pict_type = ff::AVPictureType::AV_PICTURE_TYPE_I;
                f.flags |= ff::AV_FRAME_FLAG_KEY;
            } else {
                f.pict_type = ff::AVPictureType::AV_PICTURE_TYPE_NONE;
                f.flags &= !ff::AV_FRAME_FLAG_KEY;
            }

            let send = match &mut self.input {
                Input::Software => self.frame.0,
                Input::Upload { frames, hw_frame } => {
                    ff::av_frame_unref(hw_frame.0);
                    check(
                        ff::av_hwframe_get_buffer(frames.0, hw_frame.0, 0),
                        "av_hwframe_get_buffer",
                    )?;
                    check(
                        ff::av_hwframe_transfer_data(hw_frame.0, self.frame.0, 0),
                        "upload frame",
                    )?;
                    check(
                        ff::av_frame_copy_props(hw_frame.0, self.frame.0),
                        "av_frame_copy_props",
                    )?;
                    hw_frame.0
                }
            };
            check(
                ff::avcodec_send_frame(self.ctx.0, send),
                "avcodec_send_frame",
            )?;
            // Most hardware encoders return the packet right away. Some
            // (VideoToolbox) finish asynchronously: give them a moment, and
            // let `poll` pick up anything later rather than stall capture.
            // The very first frame can take much longer (session start-up).
            let wait = if self.frames_out == 0 { 1000 } else { 4 };
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait);
            loop {
                let n = self.drain(emit)?;
                self.frames_out += n as u64;
                if n > 0 || std::time::Instant::now() >= deadline {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
    }

    /// Emits packets an asynchronous encoder finished since the last call.
    pub fn poll(&mut self, mut emit: impl FnMut(&[u8], bool)) -> Result<()> {
        let n = unsafe { self.drain(&mut emit)? };
        self.frames_out += n as u64;
        Ok(())
    }

    /// Emits every packet that is ready; returns how many there were.
    unsafe fn drain(&mut self, emit: &mut impl FnMut(&[u8], bool)) -> Result<usize> {
        let mut n = 0;
        loop {
            let ret = unsafe { ff::avcodec_receive_packet(self.ctx.0, self.pkt.0) };
            if ret == EAGAIN || ret == ff::AVERROR_EOF {
                return Ok(n);
            }
            check(ret, "avcodec_receive_packet")?;
            let p = unsafe { &*self.pkt.0 };
            let data = unsafe { std::slice::from_raw_parts(p.data, p.size as usize) };
            emit(data, p.flags & ff::AV_PKT_FLAG_KEY != 0);
            unsafe { ff::av_packet_unref(self.pkt.0) };
            n += 1;
        }
    }
}

/// Input pixel formats an encoder accepts (empty: unknown / any).
#[cfg(ffmpeg_7_1)]
unsafe fn supported_pix_fmts(codec: *const ff::AVCodec) -> Vec<Pix> {
    let mut out: *const std::ffi::c_void = std::ptr::null();
    let mut n = 0;
    let ret = unsafe {
        ff::avcodec_get_supported_config(
            std::ptr::null(),
            codec,
            ff::AVCodecConfig::AV_CODEC_CONFIG_PIX_FORMAT,
            0,
            &mut out,
            &mut n,
        )
    };
    if ret < 0 || out.is_null() {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(out as *const Pix, n as usize) }.to_vec()
}

/// Before FFmpeg 7.1 (Ubuntu 24.04 ships 6.1) the list is a field.
#[cfg(not(ffmpeg_7_1))]
unsafe fn supported_pix_fmts(codec: *const ff::AVCodec) -> Vec<Pix> {
    let mut p = unsafe { (*codec).pix_fmts };
    let mut out = Vec::new();
    if p.is_null() {
        return out;
    }
    unsafe {
        while *p != Pix::AV_PIX_FMT_NONE {
            out.push(*p);
            p = p.add(1);
        }
    }
    out
}

/// Names of every encoder in [`candidates`] that this FFmpeg build contains
/// (not necessarily usable on this machine's hardware).
pub fn compiled_encoders(codec: Codec) -> Vec<&'static str> {
    candidates(codec)
        .into_iter()
        .filter(|n| {
            let c = CString::new(*n).unwrap();
            !unsafe { ff::avcodec_find_encoder_by_name(c.as_ptr()) }.is_null()
        })
        .collect()
}

/// Human-readable FFmpeg version string.
pub fn ffmpeg_version() -> String {
    unsafe {
        CStr::from_ptr(ff::av_version_info())
            .to_string_lossy()
            .into_owned()
    }
}

/// A sensible default bitrate for a resolution / refresh / codec.
pub fn default_bitrate_kbps(codec: Codec, width: u32, height: u32, refresh_mhz: u32) -> u32 {
    let bits_per_pixel = match codec {
        Codec::H264 => 0.10,
        Codec::Hevc => 0.07,
        Codec::Av1 => 0.06,
    };
    let fps = refresh_mhz as f64 / 1000.0;
    let kbps = width as f64 * height as f64 * fps * bits_per_pixel / 1000.0;
    (kbps as u32).clamp(5_000, 200_000)
}
