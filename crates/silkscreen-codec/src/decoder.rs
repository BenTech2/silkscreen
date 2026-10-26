//! Low-latency decoding, hardware accelerated where available. Decoded
//! pictures are always handed out as NV12 or I420 in CPU memory, ready to be
//! uploaded as textures.

use std::ptr;

use anyhow::{Result, bail};
use ff::{AVHWDeviceType as Hw, AVPixelFormat as Pix};
use ffmpeg_sys_next as ff;
use silkscreen_proto::Codec;
use tracing::{debug, info, warn};

use crate::ff::{
    CodecCtx, EAGAIN, Frame, Packet, check, create_hw_device, hw_device_name, init_logging,
};

fn hw_preference() -> &'static [Hw] {
    #[cfg(target_os = "linux")]
    return &[
        Hw::AV_HWDEVICE_TYPE_CUDA,
        Hw::AV_HWDEVICE_TYPE_VAAPI,
        Hw::AV_HWDEVICE_TYPE_VULKAN,
    ];
    #[cfg(windows)]
    return &[
        Hw::AV_HWDEVICE_TYPE_D3D11VA,
        Hw::AV_HWDEVICE_TYPE_CUDA,
        Hw::AV_HWDEVICE_TYPE_DXVA2,
    ];
    #[cfg(target_os = "macos")]
    return &[Hw::AV_HWDEVICE_TYPE_VIDEOTOOLBOX];
    #[allow(unreachable_code)]
    &[]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureFormat {
    /// Y plane + interleaved UV plane.
    Nv12,
    /// Y, U, V planes.
    I420,
    /// Y, U, V planes, all full size (4:4:4).
    I444,
    /// Y plane + interleaved UV plane, both full size (4:4:4; what
    /// VideoToolbox decodes 4:4:4 to).
    Nv24,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
}

/// A decoded picture borrowed from the decoder until the next `decode` call.
pub struct Picture<'a> {
    pub format: PictureFormat,
    pub width: u32,
    pub height: u32,
    pub matrix: Matrix,
    pub full_range: bool,
    /// `(data, stride)` for each plane: 2 for NV12, 3 for I420.
    pub planes: Vec<(&'a [u8], usize)>,
}

/// An FFmpeg decoder.
struct FfDecoder {
    ctx: CodecCtx,
    hw_pix_fmt: Option<Pix>,
    pkt: Packet,
    frame: Frame,
    sw_frame: Frame,
    /// Frame that holds the newest output (either `frame` or `sw_frame`).
    have_picture: bool,
    name: String,
}

unsafe impl Send for FfDecoder {}

impl FfDecoder {
    fn open(codec: Codec, allow_hw: bool) -> Result<Self> {
        init_logging();
        let id = match codec {
            Codec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
            Codec::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
            Codec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
            Codec::PyroWave => bail!("PyroWave is not an FFmpeg codec"),
        };
        unsafe {
            // FFmpeg's default AV1 decoder is libdav1d (software only); the
            // native "av1" decoder is the one that drives hardware.
            let dec = match codec {
                Codec::Av1 if allow_hw => {
                    let native = ff::avcodec_find_decoder_by_name(c"av1".as_ptr());
                    if native.is_null() {
                        ff::avcodec_find_decoder(id)
                    } else {
                        native
                    }
                }
                _ => ff::avcodec_find_decoder(id),
            };
            if dec.is_null() {
                bail!("no {} decoder in this FFmpeg build", codec.name());
            }
            if allow_hw {
                for &kind in hw_preference() {
                    match Self::open_with(dec, Some(kind)) {
                        Ok(d) => {
                            info!(decoder = %d.name, "opened {} decoder", codec.name());
                            return Ok(d);
                        }
                        Err(e) => debug!("{} decode unavailable: {e:#}", hw_device_name(kind)),
                    }
                }
                // Stateful V4L2 decoders (e.g. the Raspberry Pi 4's H.264
                // block) are separate decoders rather than hwaccels.
                #[cfg(target_os = "linux")]
                {
                    let name = match codec {
                        Codec::H264 => c"h264_v4l2m2m",
                        Codec::Hevc => c"hevc_v4l2m2m",
                        Codec::Av1 => c"av1_v4l2m2m",
                        Codec::PyroWave => unreachable!(),
                    };
                    let m2m = ff::avcodec_find_decoder_by_name(name.as_ptr());
                    if !m2m.is_null() {
                        match Self::open_with(m2m, None) {
                            Ok(d) => {
                                info!(decoder = %d.name, "opened {} decoder", codec.name());
                                return Ok(d);
                            }
                            Err(e) => debug!("V4L2 decode unavailable: {e:#}"),
                        }
                    }
                }
            }
            let d = Self::open_with(dec, None)?;
            info!(decoder = %d.name, "opened {} decoder", codec.name());
            Ok(d)
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    unsafe fn open_with(dec: *const ff::AVCodec, hw: Option<Hw>) -> Result<Self> {
        unsafe {
            let hw_pix_fmt = match hw {
                Some(kind) => Some(find_hw_pix_fmt(dec, kind)?),
                None => None,
            };
            let ctx = CodecCtx(ff::avcodec_alloc_context3(dec));
            if ctx.0.is_null() {
                bail!("avcodec_alloc_context3 failed");
            }
            let c = &mut *ctx.0;
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            if let Some(kind) = hw {
                let device = create_hw_device(kind)?;
                // The context takes its own reference; ours drops here.
                c.hw_device_ctx = ff::av_buffer_ref(device.0);
                c.thread_count = 1;
            } else {
                // Frame threading adds a frame of latency per thread; slice
                // threading does not.
                c.thread_count = 0;
                c.thread_type = ff::FF_THREAD_SLICE;
            }
            check(
                ff::avcodec_open2(ctx.0, dec, ptr::null_mut()),
                "avcodec_open2",
            )?;
            let name = match hw {
                Some(kind) => format!("{} ({})", cstr((*dec).name), hw_device_name(kind)),
                None => format!("{} (software)", cstr((*dec).name)),
            };
            Ok(FfDecoder {
                ctx,
                hw_pix_fmt,
                pkt: Packet::new()?,
                frame: Frame::new()?,
                sw_frame: Frame::new()?,
                have_picture: false,
                name,
            })
        }
    }

    /// Feeds one complete access unit and returns the newest decoded picture,
    /// if this packet produced one.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Picture<'_>>> {
        unsafe {
            // Copy into a padded, refcounted packet as FFmpeg requires.
            check(
                ff::av_new_packet(self.pkt.0, data.len() as i32),
                "av_new_packet",
            )?;
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt.0).data, data.len());
            let ret = ff::avcodec_send_packet(self.ctx.0, self.pkt.0);
            ff::av_packet_unref(self.pkt.0);
            if ret < 0 && ret != EAGAIN {
                check(ret, "avcodec_send_packet")?;
            }

            let mut got = false;
            loop {
                let ret = ff::avcodec_receive_frame(self.ctx.0, self.frame.0);
                if ret == EAGAIN || ret == ff::AVERROR_EOF {
                    break;
                }
                check(ret, "avcodec_receive_frame")?;
                got = true;
                if Some((*self.frame.0).format) == self.hw_pix_fmt.map(|p| p as i32) {
                    ff::av_frame_unref(self.sw_frame.0);
                    check(
                        ff::av_hwframe_transfer_data(self.sw_frame.0, self.frame.0, 0),
                        "download frame",
                    )?;
                    ff::av_frame_copy_props(self.sw_frame.0, self.frame.0);
                    ff::av_frame_unref(self.frame.0);
                    self.have_picture = true;
                } else {
                    // Software output: swap so the picture lives in sw_frame.
                    std::mem::swap(&mut self.frame, &mut self.sw_frame);
                    ff::av_frame_unref(self.frame.0);
                    self.have_picture = true;
                }
            }
            if !got || !self.have_picture {
                return Ok(None);
            }
            self.picture()
        }
    }

    unsafe fn picture(&self) -> Result<Option<Picture<'_>>> {
        let f = unsafe { &*self.sw_frame.0 };
        let format = if f.format == Pix::AV_PIX_FMT_NV12 as i32 {
            PictureFormat::Nv12
        } else if f.format == Pix::AV_PIX_FMT_YUV420P as i32
            || f.format == Pix::AV_PIX_FMT_YUVJ420P as i32
        {
            PictureFormat::I420
        } else if f.format == Pix::AV_PIX_FMT_YUV444P as i32
            || f.format == Pix::AV_PIX_FMT_YUVJ444P as i32
        {
            PictureFormat::I444
        } else if f.format == Pix::AV_PIX_FMT_NV24 as i32 {
            PictureFormat::Nv24
        } else {
            warn!("unsupported decoded pixel format {}", f.format);
            return Ok(None);
        };
        let h = f.height as usize;
        let rows = |i: usize| {
            if i == 0 || matches!(format, PictureFormat::I444 | PictureFormat::Nv24) {
                h
            } else {
                h.div_ceil(2)
            }
        };
        let n = if matches!(format, PictureFormat::Nv12 | PictureFormat::Nv24) {
            2
        } else {
            3
        };
        let planes = (0..n)
            .map(|i| {
                let stride = f.linesize[i] as usize;
                (
                    unsafe { std::slice::from_raw_parts(f.data[i], stride * rows(i)) },
                    stride,
                )
            })
            .collect();
        Ok(Some(Picture {
            format,
            width: f.width as u32,
            height: f.height as u32,
            matrix: match f.colorspace {
                ff::AVColorSpace::AVCOL_SPC_BT470BG
                | ff::AVColorSpace::AVCOL_SPC_SMPTE170M
                | ff::AVColorSpace::AVCOL_SPC_FCC => Matrix::Bt601,
                _ => Matrix::Bt709,
            },
            full_range: f.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG
                || f.format == Pix::AV_PIX_FMT_YUVJ420P as i32
                || f.format == Pix::AV_PIX_FMT_YUVJ444P as i32,
            planes,
        }))
    }
}

unsafe fn find_hw_pix_fmt(dec: *const ff::AVCodec, kind: Hw) -> Result<Pix> {
    for i in 0.. {
        let cfg = unsafe { ff::avcodec_get_hw_config(dec, i) };
        if cfg.is_null() {
            break;
        }
        let cfg = unsafe { &*cfg };
        if cfg.device_type == kind
            && cfg.methods & ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32 != 0
        {
            return Ok(cfg.pix_fmt);
        }
    }
    bail!("decoder has no {} support", hw_device_name(kind))
}

unsafe fn cstr(p: *const std::ffi::c_char) -> String {
    unsafe { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
}

/// Codecs this machine can decode, in the order a display should prefer
/// them. HEVC normally comes first (ARM boards are the exception, below): it
/// is the only widely hardware-accelerated codec
/// whose encoders and decoders go beyond 4096 pixels (e.g. a 4500x3000
/// Surface Studio panel), and it needs less bandwidth than H.264.
pub fn decodable_codecs() -> Vec<Codec> {
    init_logging();
    // ARM Linux boards (Raspberry Pi 4/5) have H.264 hardware (Pi 4) or
    // decode in software (Pi 5), where H.264 costs far less CPU than HEVC.
    let order = if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        [Codec::H264, Codec::Hevc, Codec::Av1]
    } else {
        [Codec::Hevc, Codec::H264, Codec::Av1]
    };
    #[cfg_attr(not(feature = "pyrowave"), allow(unused_mut))]
    let mut codecs: Vec<_> = order
        .into_iter()
        .filter(|c| {
            let id = match c {
                Codec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
                Codec::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
                Codec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
                Codec::PyroWave => unreachable!(),
            };
            !unsafe { ff::avcodec_find_decoder(id) }.is_null()
        })
        .collect();
    // Last: experimental, and only used when the host picks it.
    #[cfg(feature = "pyrowave")]
    if crate::pyrowave::available() {
        codecs.push(Codec::PyroWave);
    }
    codecs
}

/// A video decoder: FFmpeg's, or PyroWave.
pub struct Decoder(Backend);

enum Backend {
    Ff(FfDecoder),
    #[cfg(feature = "pyrowave")]
    PyroWave(crate::pyrowave::PyroDecoder),
}

impl Decoder {
    /// Opens a decoder, in hardware when `allow_hw` and possible. PyroWave
    /// always runs on the GPU.
    pub fn open(codec: Codec, allow_hw: bool) -> Result<Self> {
        if codec == Codec::PyroWave {
            #[cfg(feature = "pyrowave")]
            return Ok(Decoder(Backend::PyroWave(
                crate::pyrowave::PyroDecoder::open()?,
            )));
            #[cfg(not(feature = "pyrowave"))]
            bail!("this build of Silkscreen has no PyroWave");
        }
        Ok(Decoder(Backend::Ff(FfDecoder::open(codec, allow_hw)?)))
    }

    pub fn name(&self) -> &str {
        match &self.0 {
            Backend::Ff(d) => d.name(),
            #[cfg(feature = "pyrowave")]
            Backend::PyroWave(d) => d.name(),
        }
    }

    /// Feeds one complete access unit and returns the newest decoded picture,
    /// if this packet produced one.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Picture<'_>>> {
        match &mut self.0 {
            Backend::Ff(d) => d.decode(data),
            #[cfg(feature = "pyrowave")]
            Backend::PyroWave(d) => d.decode(data),
        }
    }
}
