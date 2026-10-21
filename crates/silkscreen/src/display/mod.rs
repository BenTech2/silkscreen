//! Display role on the desktop: decoding with FFmpeg ([`DecodingSink`]),
//! display capabilities, and a headless runner for testing. The windowed
//! display lives in the GUI ([`crate::app`]); the network service is shared
//! with Android in `silkscreen-core`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use silkscreen_codec::{Decoder, Picture};
use silkscreen_core::display::DisplayRole;
use silkscreen_core::service::FrameSink;
use silkscreen_proto::{DEFAULT_CONTROL_PORT, DisplayCaps, EncodedFrame, InputEvent, StreamConfig};
use tracing::{info, warn};

use crate::ctx::Ctx;

/// Where decoded pictures go: the window renderer, or a counter when headless.
pub trait PictureSink: Send + Sync {
    fn stream_started(&self, cfg: &StreamConfig, host: &str);
    fn present(&self, pic: &Picture<'_>, first_packet_at: Instant);
    fn stream_stopped(&self);
}

/// Decodes with FFmpeg (hardware where possible) into a [`PictureSink`].
pub struct DecodingSink {
    out: Arc<dyn PictureSink>,
    hw: AtomicBool,
    decoder: Mutex<Option<ActiveDecoder>>,
}

struct ActiveDecoder {
    decoder: Decoder,
    cfg: StreamConfig,
    errors: u32,
    /// The decoder was swapped mid-stream and can only start at a keyframe.
    wait_key: bool,
}

impl DecodingSink {
    pub fn new(out: Arc<dyn PictureSink>, hw: bool) -> Arc<Self> {
        Arc::new(DecodingSink {
            out,
            hw: AtomicBool::new(hw),
            decoder: Mutex::new(None),
        })
    }

    /// The decoder in use, if a stream is running.
    pub fn decoder_name(&self) -> Option<String> {
        let guard = self.decoder.lock().unwrap();
        guard.as_ref().map(|a| a.decoder.name().to_owned())
    }

    /// Switches hardware decoding on or off, mid-stream if one is running.
    pub fn set_hw_decode(&self, hw: bool) {
        if self.hw.swap(hw, Ordering::Relaxed) == hw {
            return;
        }
        if let Some(a) = self.decoder.lock().unwrap().as_mut() {
            match Decoder::open(a.cfg.codec, hw) {
                Ok(d) => {
                    info!("decoding with {}", d.name());
                    a.decoder = d;
                    a.errors = 0;
                    a.wait_key = true;
                }
                Err(e) => warn!("cannot switch decoder: {e:#}"),
            }
        }
    }
}

impl FrameSink for DecodingSink {
    fn start(&self, cfg: &StreamConfig, host: &str) -> Result<String> {
        let decoder = Decoder::open(cfg.codec, self.hw.load(Ordering::Relaxed))?;
        let name = decoder.name().to_owned();
        *self.decoder.lock().unwrap() = Some(ActiveDecoder {
            decoder,
            cfg: cfg.clone(),
            errors: 0,
            wait_key: false,
        });
        self.out.stream_started(cfg, host);
        Ok(name)
    }

    fn frame(&self, frame: &EncodedFrame) -> Result<()> {
        let mut guard = self.decoder.lock().unwrap();
        let a = guard.as_mut().context("no stream")?;
        if a.wait_key {
            if !frame.keyframe {
                // The error makes the service ask the host for a keyframe.
                bail!("switched decoder; waiting for a keyframe");
            }
            a.wait_key = false;
        }
        match a.decoder.decode(&frame.data) {
            Ok(Some(pic)) => {
                a.errors = 0;
                self.out.present(&pic, frame.first_packet_at);
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) => {
                a.errors += 1;
                if a.errors >= 3 && self.hw.load(Ordering::Relaxed) {
                    // Hardware decoders can reject streams (e.g. beyond their
                    // max resolution); software always works.
                    warn!("falling back to software decoding");
                    if let Ok(d) = Decoder::open(a.cfg.codec, false) {
                        a.decoder = d;
                        a.errors = 0;
                    }
                }
                Err(e)
            }
        }
    }

    fn stop(&self) {
        *self.decoder.lock().unwrap() = None;
        self.out.stream_stopped();
    }
}

#[derive(Debug, Clone)]
pub struct DisplayOptions {
    pub port: u16,
    /// Use hardware decoding when available.
    pub hw_decode: bool,
    /// Fixed pairing code instead of a random one (tests).
    pub fixed_code: Option<String>,
    /// Skip ahead when this far behind the host (see `set_catch_up`).
    pub catch_up: Option<Duration>,
}

impl Default for DisplayOptions {
    fn default() -> Self {
        DisplayOptions {
            port: DEFAULT_CONTROL_PORT,
            hw_decode: true,
            fixed_code: None,
            catch_up: None,
        }
    }
}

pub fn caps_for(
    ctx: &Ctx,
    opts: &DisplayOptions,
    width: u32,
    height: u32,
    refresh_mhz: u32,
    scale: f32,
) -> DisplayCaps {
    let touch_points = silkscreen_platform::touch_points();
    DisplayCaps {
        name: ctx.name(),
        width: width & !1,
        height: height & !1,
        refresh_mhz,
        scale,
        physical_size_mm: None,
        touch: touch_points > 0,
        max_touch_points: touch_points,
        pen: false,
        decoders: silkscreen_codec::decodable_codecs(),
        hdr: false,
        video_port: opts.port + 1,
    }
}

#[derive(Debug, Clone)]
pub struct HeadlessSpec {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    /// Exit after this long; `None` runs forever.
    pub duration: Option<Duration>,
    /// Send synthetic pointer motion (a circle) to exercise input injection.
    pub input_test: bool,
}

struct CountingSink {
    frames: AtomicU32,
    latency_us: AtomicU32,
}

impl PictureSink for CountingSink {
    fn stream_started(&self, _cfg: &StreamConfig, _host: &str) {}
    fn present(&self, _pic: &Picture<'_>, first_packet_at: Instant) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.latency_us.fetch_add(
            first_packet_at.elapsed().as_micros() as u32,
            Ordering::Relaxed,
        );
    }
    fn stream_stopped(&self) {}
}

/// Decodes without a window — for testing and benchmarks.
pub fn run_headless(opts: DisplayOptions, ctx: Arc<Ctx>, spec: HeadlessSpec) -> Result<()> {
    let sink = Arc::new(CountingSink {
        frames: AtomicU32::new(0),
        latency_us: AtomicU32::new(0),
    });
    let caps = caps_for(&ctx, &opts, spec.width, spec.height, spec.refresh_mhz, 1.0);
    let role = DisplayRole::start(
        opts.port,
        opts.fixed_code.clone(),
        ctx,
        caps,
        DecodingSink::new(sink.clone(), opts.hw_decode),
    )?;
    role.service.set_catch_up(opts.catch_up);
    let started = Instant::now();
    let mut total = 0u64;
    loop {
        if spec.input_test {
            for i in 0..60 {
                let a = (started.elapsed().as_secs_f32() + i as f32 / 60.0) * std::f32::consts::TAU
                    / 2.0;
                role.service.send_input(InputEvent::MouseMove {
                    x: 0.5 + 0.3 * a.cos(),
                    y: 0.5 + 0.3 * a.sin(),
                });
                std::thread::sleep(Duration::from_millis(16));
            }
        } else {
            std::thread::sleep(Duration::from_secs(1));
        }
        let n = sink.frames.swap(0, Ordering::Relaxed);
        let lat = sink.latency_us.swap(0, Ordering::Relaxed);
        total += n as u64;
        if n > 0 {
            info!(
                "decoded {n} fps, receive to decoded {:.2} ms avg",
                lat as f64 / n as f64 / 1000.0
            );
        }
        if spec.duration.is_some_and(|d| started.elapsed() >= d) {
            info!("decoded {total} frames in total");
            return Ok(());
        }
    }
}
