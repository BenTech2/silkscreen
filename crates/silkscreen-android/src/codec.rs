//! Hardware decoding with MediaCodec, straight onto the app's `Surface`:
//! decoded frames never touch the CPU.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use silkscreen_core::service::FrameSink;
use silkscreen_proto::{Codec, EncodedFrame, StreamConfig};
use tracing::{debug, info, warn};

use crate::nal::parameter_sets;

const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;

fn mime(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "video/avc",
        Codec::Hevc => "video/hevc",
        Codec::Av1 => "video/av01",
    }
}

/// AMediaCodec may be fed from one thread and drained from another.
struct SharedCodec(MediaCodec);
unsafe impl Send for SharedCodec {}
unsafe impl Sync for SharedCodec {}

struct Running {
    codec: Arc<SharedCodec>,
    stop: Arc<AtomicBool>,
    drain: Option<std::thread::JoinHandle<()>>,
    /// Frames decoded before the first keyframe would be garbage.
    awaiting_keyframe: bool,
    name: String,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.drain.take() {
            let _ = t.join();
        }
        let _ = self.codec.0.stop();
    }
}

/// The video's size, for laying out the surface with the right aspect ratio.
#[derive(Debug, Clone, Copy, Default)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub streaming: bool,
}

pub struct CodecSink {
    surface: Mutex<Option<NativeWindow>>,
    stream: Mutex<Option<StreamConfig>>,
    running: Mutex<Option<Running>>,
    pub video: Mutex<VideoInfo>,
    /// When the last frame was shown (for "keep awake while active").
    last_frame: Arc<Mutex<Option<Instant>>>,
    frames: Arc<AtomicU64>,
}

impl CodecSink {
    pub fn new() -> Arc<Self> {
        Arc::new(CodecSink {
            surface: Mutex::new(None),
            stream: Mutex::new(None),
            running: Mutex::new(None),
            video: Mutex::new(VideoInfo::default()),
            last_frame: Arc::new(Mutex::new(None)),
            frames: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn since_last_frame(&self) -> Option<Duration> {
        self.last_frame.lock().unwrap().map(|t| t.elapsed())
    }

    /// The app's surface appeared (`Some`) or is going away (`None`).
    pub fn set_surface(&self, surface: Option<NativeWindow>) {
        // Tear down first: the codec must never outlive its surface.
        *self.running.lock().unwrap() = None;
        *self.surface.lock().unwrap() = surface;
        if let Some(cfg) = self.stream.lock().unwrap().clone()
            && let Err(e) = self.open(&cfg)
        {
            warn!("cannot start the decoder: {e:#}");
        }
    }

    /// Creates and starts a decoder for `cfg` on the current surface.
    fn open(&self, cfg: &StreamConfig) -> Result<String> {
        let surface = self.surface.lock().unwrap();
        let Some(window) = surface.as_ref() else {
            return Ok("MediaCodec (waiting for the screen)".into());
        };
        let codec = MediaCodec::from_decoder_type(mime(cfg.codec))
            .with_context(|| format!("no {} decoder on this device", cfg.codec.name()))?;
        let mut format = MediaFormat::new();
        format.set_str("mime", mime(cfg.codec));
        format.set_i32("width", cfg.width as i32);
        format.set_i32("height", cfg.height as i32);
        format.set_i32("max-width", cfg.width as i32);
        format.set_i32("max-height", cfg.height as i32);
        // Low-latency hints: the standard key (Android 11+), realtime
        // priority, and vendor extensions decoders otherwise ignore.
        format.set_i32("low-latency", 1);
        format.set_i32("priority", 0);
        format.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
        format.set_i32("vendor.qti-ext-dec-picture-order.enable", 1);
        format.set_i32("vendor.rtc-ext-dec-low-latency.enable", 1);
        format.set_i32("vendor.low-latency.enable", 1);
        codec
            .configure(&format, Some(window), MediaCodecDirection::Decoder)
            .map_err(|e| anyhow!("configure: {e:?}"))?;
        codec.start().map_err(|e| anyhow!("start: {e:?}"))?;
        let name = codec.name().unwrap_or_else(|_| "unknown".into());
        info!(
            "MediaCodec {name} decoding {}x{} {}",
            cfg.width,
            cfg.height,
            cfg.codec.name()
        );

        let codec = Arc::new(SharedCodec(codec));
        let stop = Arc::new(AtomicBool::new(false));
        let drain = {
            let (codec, stop, last, frames) = (
                codec.clone(),
                stop.clone(),
                self.last_frame.clone(),
                self.frames.clone(),
            );
            std::thread::Builder::new()
                .name("mediacodec-out".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match codec.0.dequeue_output_buffer(Duration::from_millis(10)) {
                            Ok(DequeuedOutputBufferInfoResult::Buffer(buf)) => {
                                // Show it now: holding frames back only adds latency.
                                let _ = codec.0.release_output_buffer(buf, true);
                                *last.lock().unwrap() = Some(Instant::now());
                                frames.fetch_add(1, Ordering::Relaxed);
                            }
                            Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                                debug!("decoder output format: {:?}", codec.0.output_format());
                            }
                            Ok(_) => {}
                            Err(e) => {
                                debug!("dequeue output: {e:?}");
                                std::thread::sleep(Duration::from_millis(5));
                            }
                        }
                    }
                })?
        };
        drop(surface);
        *self.running.lock().unwrap() = Some(Running {
            codec,
            stop,
            drain: Some(drain),
            awaiting_keyframe: true,
            name: name.clone(),
        });
        Ok(format!("MediaCodec {name}"))
    }

    fn queue(codec: &MediaCodec, data: &[u8], pts: u64, flags: u32) -> Result<()> {
        match codec.dequeue_input_buffer(Duration::from_millis(50)) {
            Ok(DequeuedInputBufferResult::Buffer(mut buf)) => {
                let dst = buf.buffer_mut();
                if dst.len() < data.len() {
                    bail!(
                        "frame of {} bytes exceeds the decoder's buffer ({})",
                        data.len(),
                        dst.len()
                    );
                }
                for (d, s) in dst.iter_mut().zip(data) {
                    d.write(*s);
                }
                codec
                    .queue_input_buffer(buf, 0, data.len(), pts, flags)
                    .map_err(|e| anyhow!("queue input: {e:?}"))
            }
            Ok(DequeuedInputBufferResult::TryAgainLater) => bail!("decoder is busy"),
            Err(e) => bail!("dequeue input: {e:?}"),
        }
    }
}

impl FrameSink for CodecSink {
    fn start(&self, cfg: &StreamConfig, _host: &str) -> Result<String> {
        *self.stream.lock().unwrap() = Some(cfg.clone());
        *self.video.lock().unwrap() = VideoInfo {
            width: cfg.width,
            height: cfg.height,
            streaming: true,
        };
        *self.running.lock().unwrap() = None;
        self.open(cfg)
    }

    fn frame(&self, frame: &EncodedFrame) -> Result<()> {
        let mut running = self.running.lock().unwrap();
        let r = running.as_mut().context("no decoder (screen not ready)")?;
        if r.awaiting_keyframe {
            if !frame.keyframe {
                bail!("waiting for a keyframe");
            }
            let codec = self
                .stream
                .lock()
                .unwrap()
                .as_ref()
                .map_or(Codec::Hevc, |c| c.codec);
            if let Some(params) = parameter_sets(codec, &frame.data) {
                Self::queue(&r.codec.0, params, frame.pts_us, BUFFER_FLAG_CODEC_CONFIG)?;
            }
            r.awaiting_keyframe = false;
            debug!("decoder {} synced on keyframe {}", r.name, frame.frame_id);
        }
        Self::queue(&r.codec.0, &frame.data, frame.pts_us, 0)
    }

    fn stop(&self) {
        *self.running.lock().unwrap() = None;
        *self.stream.lock().unwrap() = None;
        self.video.lock().unwrap().streaming = false;
    }
}
